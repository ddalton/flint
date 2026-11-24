#!/bin/bash
# swap190 (10-30 c27, the user: "we should then allocate 190 cores for the next run?"): when XLTNJ's result line
# lands, stop run.sh (its process group) and the 160-worker XCTNJ it starts, and run XCTNJ here with 190 workers
# in run.sh's arm format; endafter.sh (waiting for XCTNJ's line) then does the final sync and the shutdown.
BK=flint-c27-20261030; W=/work; S=$W/formal/ms12; MC=/root/.cargo/bin/tla-mc; O=$W/out/c27.txt
export HOME=/root; . /root/.cargo/env; export CARGO_INCREMENTAL=0
until grep -qE '^[A-Z]+ XLTNJ ' $O; do sleep 1; done
rs=$(ps -eo pid,args | grep "[b]ash /work/run.sh" | awk 'NR==1{print $1}')
[ -n "$rs" ] && kill -TERM -- -$(ps -o pgid= -p $rs | tr -d ' ')
sleep 1
for p in $(ps -eo pid,args | grep "[m]eta-c27-FlintCatchupXCTNJ" | awk '{print $1}'); do kill $p; done
sleep 3; rm -rf $W/meta-c27-FlintCatchupXCTNJ
echo "SWAP $(date -u +%TZ): run.sh stopped after XLTNJ; XCTNJ runs from swap190.sh with 190 workers (the user's word)" >> $O
( while true; do aws s3 sync $W/out s3://$BK/out --quiet --exclude '*meta-*' --exclude '*cache-*'; sleep 300; done ) &
( while true; do { date -u +%TZ; cut -d' ' -f1 /proc/loadavg; free -g | sed -n 2p; df -h $W | tail -1; echo "tlapm $(pgrep -x tlapm | wc -l) z3 $(pgrep -x z3 | wc -l)"; } >> $W/out/res.txt; sleep 120; done ) &
c=FlintCatchupXCTNJ; cap=7200; t0=$(date +%s); cd $S
( timeout $cap env TLAMC_CACHE=$W/cache-c27 $MC -compile -workers 190 -fpmem 180000 -queue-mem 180000 -checkpoint 0 -metadir $W/meta-c27-$c -config $c.cfg FlintCatchupS.tla ) > $W/out/c27-$c.out 2>&1
rc=$?; rm -rf $W/meta-c27-$c
got=$(grep -m1 -oE "No error has been found|Invariant [A-Za-z_0-9]+ is violated|Temporal propert[a-z]* [A-Za-z_]* *(is|were) violated|tla-mc: [a-z]+:.*|[Ee]rror:.*|No space left on device" $W/out/c27-$c.out | head -1)
case "$rc:$got" in 0:No*) v=HOLD;; *nvariant*violated*|*emporal*violated*) v=VIOL;; 124:*) v=NOVERDICT;; *) v=ERROR;; esac
echo "$v XCTNJ (FlintCatchupS.tla) rc=$rc got=[$got] $(grep -oE "[0-9]+ distinct states found\. Depth [0-9]+" $W/out/c27-$c.out | tail -1) $(grep -oE "depth [0-9]+, [0-9]+ generated, [0-9]+ distinct" $W/out/c27-$c.out | tail -1) $(( $(date +%s) - t0 ))s cap=${cap}s" >> $O
