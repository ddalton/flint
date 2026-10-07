#!/bin/bash
# Restart 2026-10-06 ~14:06Z: run 1 died at depth 26 (983,406,036 distinct) with "memory allocation of 178749 bytes
# failed" while 1.08 TB was free — the process held 65,531 maps = vm.max_map_count (65530). Raised; queue budget
# lowered to 150 GB (run 1 used ~412 GB RSS against a 250 GB budget at depth 26).
export HOME=/root AWS_DEFAULT_REGION=us-west-1; ulimit -n 1048576
sysctl -w vm.max_map_count=16777216
BK=flint-tlc-lean-20261006; O=/data/out; LF=/opt/flint/lean/formal; W=LeanP1Size3L4W3
log() { echo "$(date -u +%FT%TZ) $*" | tee -a $O/RESULTS.txt; }
( while true; do aws s3 cp $O/ s3://$BK/out/ --recursive --quiet; sleep 60; done ) &
( while true; do P=$(pgrep tlcgen); echo "$(date -u +%T) $(free -g | awk '/Mem/{print "used",$3,"avail",$7}') $(df -BG --output=used /data | tail -1) maps ${P:+$(wc -l < /proc/$P/maps)}" >> $O/mem.log; sleep 60; done ) &
mv $O/$W.out $O/$W.run1.out; rm -rf /data/st/a
BIN=$(ls /data/gt/release/tlcgen-* | grep -v '\.d$' | head -1); t0=$(date +%s)
log "run 1 failed: map count 65531 = vm.max_map_count at depth 26, 983,406,036 distinct, no violation; restarting with max_map_count=$(sysctl -n vm.max_map_count), -queue-mem 150000"
(cd $LF && timeout 18000 $BIN -workers 192 -checkpoint 0 -metadir /data/st/a -fpmem 200000 -queue-mem 150000 -config $W.cfg LeanP1.tla > $O/$W.out 2>&1); rc=$?
log "$W rc=$rc $(( $(date +%s)-t0 ))s (124 = 5 h cap) | $(grep -oE 'No error has been found|Invariant [A-Za-z_]+ is violated|Action property [A-Za-z_]+ is violated|Error:.*|memory allocation.*' $O/$W.out | head -1) | $(grep -E 'states generated|^progress' $O/$W.out | tail -1 | cut -c1-170)"
rm -rf /data/st/a
log DONE; echo DONE > $O/DONE; aws s3 cp $O/ s3://$BK/out/ --recursive --quiet
shutdown -h +5
