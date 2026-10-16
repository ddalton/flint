#!/bin/bash
# run-shards.sh MOD N THREADS: one module as N tlapm processes, each on its own
# line range (shard-ranges.py) in its own directory (no shared .tlacache).
# Run from a directory holding the module chain.  Writes sh-MOD/S<i>.out and
# sh-MOD/SUMMARY.txt when all shards end.  Flags as the record (smt, stretch 12);
# METHOD= overrides (L1 needs the default methods: METHOD="").
m=$1; n=$2; th=${3:-1}; T=/mnt/nvme/tlaps/unpack/tlapm/bin/tlapm
METHOD=${METHOD---method smt}
D=$PWD/sh-$m; rm -rf $D; mkdir -p $D
python3 $(dirname $0)/shard-ranges.py LeanP1Proof$m.tla $n > $D/ranges.txt
md5sum *.tla > $D/md5.txt
i=0
while read a b; do
  i=$((i+1)); mkdir -p $D/s$i; cp *.tla $D/s$i/
  ( cd $D/s$i && /usr/bin/time -f "WALL %e s MAXRSS %M kB rc %x" nice -n 5 $T --toolbox $a $b --cleanfp $METHOD --stretch 12 --threads $th -k LeanP1Proof$m.tla > $D/S$i.out 2>&1
    echo "SHARD $i lines $a-$b: $(grep -h 'obligations' $D/S$i.out | tail -1) $(grep WALL $D/S$i.out)" >> $D/shards.txt ) &
done < $D/ranges.txt
wait
{ echo "MODULE $m $(date -u +%FT%TZ) $i shards"; sort -n -k2 $D/shards.txt
  echo "failed: $(cat $D/S*.out | grep -c status:failed)"
  for f in $D/S*.out; do python3 - $f <<'PY'
import sys,re
for b in open(sys.argv[1]).read().split('@!!BEGIN'):
    if '@!!status:failed' in b:
        loc=re.search(r'@!!loc:(\d+)',b); r=re.search(r'@!!reason:(.*)',b); mt=re.search(r'@!!meth:(.*)',b)
        print('  FAIL line', loc.group(1), (r.group(1) if r else ''), (mt.group(1) if mt else ''))
PY
  done; } > $D/SUMMARY.txt
echo SHARDSDONE >> $D/SUMMARY.txt
