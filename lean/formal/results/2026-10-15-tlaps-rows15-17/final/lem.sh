#!/bin/bash
# lem.sh TAG MODULE : one tlapm per "name a b" line of in2/TAG/ranges.txt, on the
# texts in in2/TAG (over /work/proof); summary to out/TAG/SUMMARY.txt.
export HOME=/root; . /root/.cargo/env; W=/work; TAG=$1; M=$2; B=s3://flint-tlc-m11-20261016/in2/$TAG
TL=$W/tlaps/tlapm/bin/tlapm; R=$W/$TAG; O=$W/out/$TAG; rm -rf $R $O; mkdir -p $R/src $O
cp $W/proof/*.tla $R/src/; aws s3 cp $B $R/src --recursive --quiet; md5sum $R/src/*.tla > $O/md5.txt
while read n a b M; do d=$R/$n; mkdir -p $d; cp $R/src/*.tla $d/
  ( cd $d && /usr/bin/time -f "WALL %e s rc %x" $TL --toolbox $a $b --cleanfp --method smt --stretch 24 --threads 8 -k $M < /dev/null > $O/$n.out 2>&1 ) &
done < $R/src/ranges.txt
DL=$(( $(date +%s) + 60*60 ))
while true; do left=0; for p in $(pgrep -x tlapm); do case "$(readlink /proc/$p/cwd)" in $R/*) left=1;; esac; done; [ $left = 0 ] && break
  if [ $(date +%s) -ge $DL ]; then for p in $(pgrep -x tlapm); do case "$(readlink /proc/$p/cwd)" in $R/*) kill $p;; esac; done; echo DEADLINE >> $O/SUMMARY.txt; sleep 5; break; fi; sleep 15; done
for f in $O/*.out; do echo "$(basename $f .out): $(grep -c status:proved $f) proved $(grep -c status:failed $f) failed / $(grep -A1 obligationsnumber $f | grep -o '[0-9]*' | tail -1) $(grep -o 'WALL.*' $f) $(grep -c 'type:error' $f) err"; done >> $O/SUMMARY.txt
echo "DONE $(date -u +%TZ)" >> $O/SUMMARY.txt; aws s3 sync $O s3://flint-tlc-m11-20261016/out/$TAG --quiet
