#!/bin/bash
# EC2 2026-10-16 (user: "complete proof by taking advantage of the EC2 box"):
# c7i.48xlarge.  ONE script, no hand-offs; userdata's `shutdown -h +180` is the
# hard cap.  It does NOT shut down at the end: the slack is for one fix round
# over SSM, then a manual teardown.  In order:
#  P. M11 (AK1 part 2) and M12 (its side invariant) on today's text, sharded by
#     line range (tlapm's front end is single-threaded): M11 48, M12 16 shards;
#     40-min deadline, stragglers stopped, logs kept.
#  C. the TLC candidates (MCLeanP1M10), 192 workers, one at a time:
#     ZG (GwDecl, DocCarAny: GCas/GRename DocCar) 25 min; ZRS (DocCar across a
#     rename-back) 15 min; RUL2 (CPubLeft2, one sync + two touches) 10 min;
#     RUL (CPubLeft2) 5 min.
BK=$1; W=/work; F=$W/formal; P=$W/proof; cd $W; mkdir -p out/p out/c
export HOME=/root; . /root/.cargo/env
TL=$W/tlaps/tlapm/bin/tlapm
( while true; do aws s3 sync $W/out s3://$BK/out --quiet --exclude '*meta-*' --exclude '*cache-*'; sleep 60; done ) & SYNC=$!
( while true; do { date -u +%TZ; cut -d' ' -f1 /proc/loadavg; free -g | sed -n 2p; echo "tlapm $(pgrep -x tlapm | wc -l) z3 $(pgrep -x z3 | wc -l)"; } >> out/res.txt; sleep 120; done ) & MON=$!
echo "START $(date -u +%FT%TZ) $(nproc) cpus; M11 $(md5sum $P/LeanP1ProofM11.tla | cut -c1-8) M12 $(md5sum $P/LeanP1ProofM12.tla | cut -c1-8) Anc $(md5sum $P/LeanP1Anc.tla | cut -c1-8)" > out/RESULTS.txt
md5sum $P/*.tla > out/p/md5.txt
# P: module -> shards
declare -A NS=( [M11]=48 [M12]=16 )
for m in M11 M12; do
  n=${NS[$m]}; python3 $P/shard-ranges.py $P/LeanP1Proof$m.tla $n > out/p/$m-ranges.txt
  i=0
  while read a b; do i=$((i+1)); d=$W/pw/$m-$i; mkdir -p $d; cp $P/*.tla $d/
    ( cd $d && /usr/bin/time -f "WALL %e s MAXRSS %M kB rc %x" $TL --toolbox $a $b --cleanfp --method smt --stretch 12 --threads 1 -k LeanP1Proof$m.tla > $W/out/p/$m-S$i.out 2>&1 ) &
  done < out/p/$m-ranges.txt
done
echo "PROOFS launched $(date -u +%FT%TZ): $(pgrep -x tlapm | wc -l) shards" >> out/RESULTS.txt
DL=$(( $(date +%s) + 40*60 ))
while pgrep -x tlapm > /dev/null; do
  if [ $(date +%s) -ge $DL ]; then for p in $(pgrep -x tlapm); do echo "DEADLINE tlapm $p $(readlink /proc/$p/cwd)" >> out/RESULTS.txt; kill $p; done; sleep 10; break; fi
  sleep 15
done
for p in $(pgrep -x z3); do kill $p; done
# per module: obligations (toolbox count), proved, trivial, failed, errors, and the failing lines
python3 - $W/out/p >> out/RESULTS.txt <<'PY'
import sys, glob, re, os, time
d = sys.argv[1]
for m in ['M11', 'M12']:
    tot = pr = tv = fl = er = 0; fails = []; short = []
    for f in sorted(glob.glob(f'{d}/{m}-S*.out'), key=lambda x: int(re.search(r'-S(\d+)', x).group(1))):
        t = open(f, errors='replace').read()
        c = re.findall(r'@!!type:obligationsnumber\s*\n@!!count:(\d+)', t)
        n = int(c[-1]) if c else 0
        p_ = t.count('@!!status:proved'); v_ = t.count('@!!status:trivial'); f_ = t.count('@!!status:failed')
        e_ = t.count('@!!type:error')
        tot += n; pr += p_; tv += v_; fl += f_; er += e_
        if p_ + v_ + f_ < n: short.append(f"{os.path.basename(f)}({p_ + v_ + f_}/{n})")
        for b in t.split('@!!BEGIN'):
            if '@!!status:failed' in b:
                loc = re.search(r'@!!loc:(\d+)', b); r = re.search(r'@!!reason:(.*)', b)
                fails.append(f"{loc.group(1)}:{(r.group(1) if r else '').split(';')[0][:20]}")
    print(f"PROOF {m} {time.strftime('%Y-%m-%dT%H:%M:%SZ', time.gmtime())} obligations {tot} proved {pr} trivial {tv} failed {fl} errors {er} unaccounted {' '.join(short) or '-'}")
    print(f"  FAILS {m}: {' '.join(fails) or '-'}")
PY
# C: the TLC candidates
cd $F
for cw in "ZG 1500" "ZRS 900" "RUL2 600" "RUL 300"; do set -- $cw; c=$1; cap=$2; O=$W/out/c/$c.out
  ( time timeout $cap env TLAMC_CACHE=$W/cache-$c tla-mc -compile -workers 192 -fpmem 60000 -queue-mem 100000 -checkpoint 0 -metadir $W/meta-$c -config $c.cfg MCLeanP1M10.tla ) > $O 2>&1
  echo "rc=$? $(date -u +%H:%M:%SZ)" >> $O; rm -rf $W/meta-$c $W/cache-$c
  echo "CAND $c $(date -u +%FT%TZ) $(grep -h -o -E 'Invariant [A-Za-z0-9]+ is violated|No error has been found|[0-9]+ distinct states found. Depth [0-9]+|No space left on device' $O | tr '\n' ' ') $(grep -h -o 'progress: depth [0-9]*, [0-9]* generated, [0-9]* distinct' $O | tail -1)" >> $W/out/RESULTS.txt
done
cd $W; echo "DONE $(date -u +%FT%TZ) (no shutdown: the cap or a manual teardown)" >> out/RESULTS.txt
kill $SYNC $MON; aws s3 sync $W/out s3://$BK/out --quiet --exclude '*meta-*' --exclude '*cache-*'
