#!/bin/bash
# watch2's job for runner3: hold the shutdown at the 17:50Z cap while it runs.
R=/work/out/RESULTS.txt
while pgrep -f '^bash /work/runner3.sh' > /dev/null; do
  grep -q '^DONE ' $R && shutdown -h 17:50
  sleep 60
done
grep -q '^DONE ' $R && ! pgrep -f '^bash /work/runner2.sh' > /dev/null && shutdown -h +5
