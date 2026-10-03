P=$(pgrep -f "^/opt/payload/gen-target/release/tlcgen-leanp1 -workers 32 " | head -1); D=/data/out/profile2-w32; mkdir -p $D
echo "pid=$P $(ps -o args= -p $P | cut -c1-80)" > $D/pid.txt
top -H -b -n 1 -p $P | head -50 > $D/top-threads.txt
pidstat -w -t -p $P 5 2 > $D/pidstat-w.txt 2>&1
perf record -F 199 -p $P -o /data/perf2-w32.data -- sleep 20 > /dev/null 2>&1
perf report -i /data/perf2-w32.data --no-children --percent-limit 0.3 --stdio 2>/dev/null | grep -v '^#' | grep -v '^$' | head -80 > $D/perf-symbols.txt
perf report -i /data/perf2-w32.data --no-children --sort dso --stdio 2>/dev/null | grep -v '^#' | grep -v '^$' | head -20 > $D/perf-dso.txt
cat $D/pid.txt; head -30 $D/top-threads.txt | tail -24; tail -40 $D/pidstat-w.txt | grep Average | head -40; echo ===DSO; cat $D/perf-dso.txt; echo ===SYM; head -45 $D/perf-symbols.txt
