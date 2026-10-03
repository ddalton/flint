for w in 16 32; do
  until [ -f /data/out/w$w.out ]; do sleep 5; done; sleep 120
  P=$(pgrep -f "tlcgen-leanp1 -workers $w " | head -1); D=/data/out/profile-w$w; mkdir -p $D
  [ -n "$P" ] || { echo "no pid for w$w" > $D/error.txt; continue; }
  top -H -b -n 1 -p $P | head -50 > $D/top-threads.txt
  pidstat -w -t -p $P 5 2 > $D/pidstat-w.txt 2>&1
  perf record -F 199 -p $P -o /data/perf-w$w.data -- sleep 20 > /dev/null 2>&1
  perf report -i /data/perf-w$w.data --no-children --percent-limit 0.3 --stdio 2>/dev/null | grep -v '^#' | grep -v '^$' | head -80 > $D/perf-symbols.txt
  perf report -i /data/perf-w$w.data --no-children --sort dso --stdio 2>/dev/null | grep -v '^#' | grep -v '^$' | head -20 > $D/perf-dso.txt
  echo "w$w profiled $(date -u +%T)" >> /data/out/runner.log
done
