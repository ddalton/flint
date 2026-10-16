#!/bin/bash
# EC2 2026-10-15 second run (user: "provision the box ... run whatever you can";
# "1.5 hours max"): c7i.48xlarge.  ONE script, no hand-offs; userdata's
# `shutdown -h +90` is the hard cap.  In order:
#  P. the record runs of the final proof text (fix5 chain + M7 + M10), every
#     module sharded by line range (tlapm's front end is single-threaded),
#     ~110 front ends with headroom for Z3;
#  T. the four tie worlds the first run lost (LeanP1 vs LeanP1Anc, EXACT counts),
#     each pair side by side at 96 workers;
#  C. AK1 part 2's candidate invariants (MCLeanP1M10) at the M7 worlds' bounds,
#     192 workers, 15-min cap each.
BK=$1; W=/work; F=$W/formal; P=$W/proof; cd $W; mkdir -p out/p out/tie out/c
export HOME=/root; . /root/.cargo/env
TL=$W/tlaps/tlapm/bin/tlapm
( while true; do aws s3 sync $W/out s3://$BK/out --quiet --exclude '*meta-*' --exclude '*cache-*'; sleep 60; done ) & SYNC=$!
( while true; do { date -u +%TZ; cut -d' ' -f1 /proc/loadavg; free -g | sed -n 2p; echo "tlapm $(pgrep -x tlapm | wc -l) z3 $(pgrep -x z3 | wc -l)"; } >> out/res.txt; sleep 120; done ) & MON=$!
echo "START $(date -u +%FT%TZ) $(nproc) cpus; LeanP1.tla $(md5sum $F/LeanP1.tla | cut -c1-8); LeanP1Anc.tla $(md5sum $P/LeanP1Anc.tla | cut -c1-8)" > out/RESULTS.txt
md5sum $P/*.tla > out/p/md5.txt
# P: module -> shards
# Only the modules whose text changed since their last proof (M1, M3, M5C, M7-M10 proved on this text).
declare -A NS=( [M2]=32 [M0]=16 [M5D]=10 [M6]=8 )
for m in M2 M0 M5D M6; do
  n=${NS[$m]}; python3 $P/shard-ranges.py $P/LeanP1Proof$m.tla $n > out/p/$m-ranges.txt
  i=0
  while read a b; do i=$((i+1)); d=$W/pw/$m-$i; mkdir -p $d; cp $P/*.tla $d/
    ( cd $d && /usr/bin/time -f "WALL %e s MAXRSS %M kB rc %x" $TL --toolbox $a $b --cleanfp --method smt --stretch 12 --threads 1 -k LeanP1Proof$m.tla > $W/out/p/$m-S$i.out 2>&1 ) &
  done < out/p/$m-ranges.txt
done
echo "PROOFS launched $(date -u +%FT%TZ): $(pgrep -x tlapm | wc -l) shards" >> out/RESULTS.txt
wait_tlapm() { while pgrep -x tlapm > /dev/null; do sleep 15; done; }
DL=$(( $(date +%s) + 35*60 ))   # proofs get at most 35 min; stragglers stopped, logs kept
while pgrep -x tlapm > /dev/null; do
  if [ $(date +%s) -ge $DL ]; then for p in $(pgrep -x tlapm); do echo "DEADLINE tlapm $p $(pwd -P /proc/$p/cwd 2>/dev/null; readlink /proc/$p/cwd)" >> out/RESULTS.txt; kill $p; done; sleep 10; break; fi
  sleep 15
done
for p in $(pgrep -x z3); do kill $p; done
python3 - $W/out/p >> out/RESULTS.txt <<'PY'
import sys, glob, re, os, time
d = sys.argv[1]
for m in ['M2', 'M0', 'M5D', 'M6']:
    ob = fl = 0; nd = []
    for f in sorted(glob.glob(f'{d}/{m}-S*.out')):
        t = open(f).read()
        k = re.findall(r'(?:All (\d+) obligations proved|(\d+)/(\d+) obligations failed)', t)
        if not k: nd.append(os.path.basename(f)); continue
        a, x, y = k[-1]; ob += int(a or y); fl += int(x or 0)
    print(f"PROOF {m} {time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime())} obligations {ob} failed {fl} not-done {' '.join(nd) or '-'}")
PY
# T: the tie
cd $W; X="-fpmem 30000 -queue-mem 50000"; D=$W/tie; mkdir -p $D/world; cp $F/*.tla $F/*.cfg $D/world/; cp $P/LeanP1Anc.tla $D/world/
cd $D/world; md5sum LeanP1.tla LeanP1Anc.tla > $W/out/tie/md5.txt
run() { w=$1; m=$2; O=$W/out/tie/$w-$m.out
  ( time TLAMC_CACHE=$D/cache-$w-$m tla-mc -compile -workers 96 -checkpoint 0 -metadir $D/meta-$w-$m $X -config LeanP1$w.cfg $m.tla ) > $O 2>&1
  echo "rc=$? $(date -u +%H:%M:%SZ)" >> $O; rm -rf $D/meta-$w-$m $D/cache-$w-$m; }
for w in ScopeHolds DeposeHolds AckCap ResHolds; do
  run $w LeanP1 & run $w LeanP1Anc & wait
  a=$(grep -h -o -E '[0-9]+ states generated, [0-9]+ distinct states found' $W/out/tie/$w-LeanP1.out)
  b=$(grep -h -o -E '[0-9]+ states generated, [0-9]+ distinct states found' $W/out/tie/$w-LeanP1Anc.out)
  v=$( [ -n "$a" ] && [ "$a" = "$b" ] && echo EXACT || echo DIFFER )
  echo "TIE $w $v | orig: $a $(grep -h -o -E 'No error has been found|is violated' $W/out/tie/$w-LeanP1.out) | anc: $b $(grep -h -o -E 'No error has been found|is violated' $W/out/tie/$w-LeanP1Anc.out)" >> $W/out/RESULTS.txt
done
# C: AK1 part 2 candidates
cd $F
for c in M10AckRename M10AckRescopeRename2; do O=$W/out/c/$c.out
  ( time timeout 900 env TLAMC_CACHE=$W/cache-c tla-mc -compile -workers 192 -fpmem 60000 -queue-mem 100000 -checkpoint 0 -metadir $W/meta-c -config $c.cfg MCLeanP1M10.tla ) > $O 2>&1
  echo "rc=$? $(date -u +%H:%M:%SZ)" >> $O; rm -rf $W/meta-c $W/cache-c
  echo "CAND $c $(date -u +%FT%TZ) $(grep -h -o -E 'Invariant [A-Za-z0-9]+ is violated|No error has been found|[0-9]+ distinct states found. Depth [0-9]+' $O | tr '\n' ' ') $(grep -h -o 'progress: depth [0-9]*, [0-9]* generated, [0-9]* distinct' $O | tail -1)" >> $W/out/RESULTS.txt
done
cd $W; echo "DONE $(date -u +%FT%TZ)" >> out/RESULTS.txt
kill $SYNC $MON; aws s3 sync $W/out s3://$BK/out --quiet --exclude '*meta-*' --exclude '*cache-*'
shutdown -h +2
