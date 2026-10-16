#!/bin/bash
# EC2 2026-10-15, c7i.48xlarge (x86, 192 vCPU): the rows 15-17 proof chain on the
# L-137 model, then the new ack worlds.  User: proofs FIRST (all cores), then the
# models, compiled, one world at a time; new worlds only, keep cost low.
#  Phase P: every proof module at once (tlapm only checks the module it is given),
#           threads by module size, ~188 in all; flags as on the box (smt, stretch 12).
#  Phase T: starts when <= 3 modules are still proving, with the cores they leave:
#   1. CarrierCitesAny, L-137's mutant (must fire AK1);  2. AckRename to the end;
#   3. touch + sync / failed fetch / rescope / rescope + sync (AK1, AK3, AK4, NR1, DP1, DP2);
#   4. M7's candidate invariants (MCLeanP1M7.tla): RescopeRename2, AckRename bounds.
# out/ synced every 60 s (proof .out files, TLC logs, RESULTS.txt).
BK=$1; W=/work; F=$W/formal; P=$W/proof; cd $W; mkdir -p out/p
export HOME=/root; . /root/.cargo/env
TL=$W/tlaps/tlapm/bin/tlapm
( while true; do aws s3 sync $W/out s3://$BK/out --quiet --exclude '*meta-*' --exclude '*cache-*' --exclude '*/mut/*'; sleep 60; done ) & SYNC=$!
( while true; do { date -u +%TZ; uptime; df -h /work | tail -1; free -g | sed -n 2p; for f in out/p/*.out; do echo "$(basename $f .out) $(grep -c status:proved $f)/$(grep -c status:failed $f)"; done | tr '\n' ' '; echo; } >> out/res.txt; sleep 120; done ) & MON=$!
echo "START $(date -u +%FT%TZ) $(nproc) cpus; LeanP1.tla $(md5sum $F/LeanP1.tla | cut -c1-8); LeanP1Anc.tla $(md5sum $P/LeanP1Anc.tla | cut -c1-8)" > out/RESULTS.txt
md5sum $P/*.tla > out/p/md5.txt
declare -A TH=( [M2]=32 [M1]=24 [M5C]=24 [M0]=22 [M3]=18 [M5A]=12 [M5D]=10 [M6]=10 [M7]=8 [M8]=8 [M9]=8 [M4]=6 [M5B]=3 [L1]=3 )
declare -A PID
for m in "${!TH[@]}"; do
  d=$W/pw/$m; mkdir -p $d; cp $P/*.tla $d/
  ( cd $d; /usr/bin/time -f "WALL %e s MAXRSS %M kB rc %x" $TL --toolbox 0 0 --cleanfp --method smt --stretch 12 --threads ${TH[$m]} -k LeanP1Proof$m.tla > $W/out/p/$m.out 2>&1
    echo PROOFDONE >> $W/out/p/$m.out
    echo "PROOF $m $(date -u +%FT%TZ) proved $(grep -c status:proved $W/out/p/$m.out) failed $(grep -c status:failed $W/out/p/$m.out) $(grep WALL $W/out/p/$m.out)" >> $W/out/RESULTS.txt ) &
  PID[$m]=$!
done
busy() { local n=0 t=0; for m in "${!PID[@]}"; do kill -0 ${PID[$m]} 2>/dev/null && { n=$((n+1)); t=$((t+TH[$m])); }; done; echo "$n $t"; }
while true; do read n t < <(busy); [ $n -le 3 ] && break; sleep 30; done
TW=$((192 - t)); [ $TW -lt 96 ] && TW=96
echo "PHASE T $(date -u +%FT%TZ) $n modules still proving ($t threads); TLC workers $TW" >> out/RESULTS.txt
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
wait "${PID[@]}"
echo "DONE $(date -u +%FT%TZ)" >> out/RESULTS.txt
kill $SYNC $MON; aws s3 sync $W/out s3://$BK/out --quiet --exclude '*meta-*' --exclude '*cache-*' --exclude '*/mut/*'
shutdown -h +5
