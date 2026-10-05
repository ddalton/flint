#!/bin/bash
# Size a three-syncer LeanP1 world: the same bounds with Writers={A,B} and {A,B,C}, a ladder of levels, each to completion (15 min cap).
set -u
S=/private/tmp/claude-503/-Users-ddalton-github-flint/43d3502a-78c0-49e1-a64a-e1c9da8981bf/scratchpad
F=$S/wt-size/lean/formal; T=$S/wt-size/formal/tlc-rs/target/release/tlc-rs; O=$S/size3
md5 -q $F/LeanP1.tla | cut -c1-8 | sed 's/^/LeanP1.tla md5 /'
# level: MaxMint MaxUI MaxBarriers MaxRestarts MaxSyncs MaxCopies
LEVELS="L1:2:0:1:0:0:1 L2:2:1:2:0:0:1 L3:3:1:2:0:0:1 L4:3:1:3:0:0:1 L5:3:1:3:1:0:1"
for lv in $LEVELS; do
  IFS=: read -r name mint ui bar rst syn cop <<< "$lv"
  for W in "A, B" "A, B, C"; do
    n=$(echo "$W" | tr -cd 'ABC' | wc -c | tr -d ' ')
    cfg=$F/Size3-$name-w$n.cfg
    sed -e "s/^  Writers = .*/  Writers = {$W}/" -e "s/^  MaxMint = .*/  MaxMint = $mint/" -e "s/^  MaxUI = .*/  MaxUI = $ui/" \
        -e "s/^  MaxBarriers = .*/  MaxBarriers = $bar/" -e "s/^  MaxRestarts = .*/  MaxRestarts = $rst/" -e "s/^  MaxSyncs = .*/  MaxSyncs = $syn/" \
        -e "s/^  MaxCopies = .*/  MaxCopies = $cop/" $F/LeanP1Holds1p3b.cfg > $cfg
    t0=$(date +%s)
    ( cd $F && $T -workers 4 -config $(basename $cfg) LeanP1.tla > $O/$name-w$n.out 2>&1 ) &
    pid=$!; why=""
    while kill -0 $pid 2>/dev/null; do
      sleep 5
      rss=$(ps -o rss= -p $(pgrep -P $pid tlc-rs 2>/dev/null || echo $pid) 2>/dev/null | tr -d ' ')
      [ "${rss:-0}" -gt 5000000 ] && { why=CAP-RSS; pkill -P $pid; kill $pid 2>/dev/null; break; }
      [ $(( $(date +%s) - t0 )) -gt 900 ] && { why=CAP-15min; pkill -P $pid; kill $pid 2>/dev/null; break; }
    done
    wait $pid 2>/dev/null
    last=$(grep -E "distinct" $O/$name-w$n.out | tail -1 | cut -c1-140)
    verdict=$(grep -oE "No error has been found|Invariant [A-Za-z_]+ is violated|property [A-Za-z_]+ is violated" $O/$name-w$n.out | head -1)
    echo "$name w$n mint=$mint ui=$ui bar=$bar rst=$rst syn=$syn cop=$cop  $(( $(date +%s) - t0 ))s  ${why:-${verdict:-?}} | $last"
    rm -f $cfg
  done
done
