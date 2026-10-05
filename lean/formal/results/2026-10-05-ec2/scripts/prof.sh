dnf -y install perf >/dev/null 2>&1; p=$(pgrep -f tlcgen-forgesynckeptset | head -1); echo "pid $p"
perf record -F 49 -g -p $p -o /data/perf.data -- sleep 20 2>&1 | tail -1
perf report -i /data/perf.data --stdio --no-children --sort symbol --percent-limit 1 -g none 2>/dev/null | grep -v "^#" | grep -v "^$" | head -40
echo "--- by dso"; perf report -i /data/perf.data --stdio --no-children --sort dso -g none 2>/dev/null | grep -v "^#" | grep -v "^$" | head -8
