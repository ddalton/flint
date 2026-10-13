#!/bin/bash
# Keeps runner.sh's "shutdown +5" from killing runner2's runs: while runner2
# runs, the shutdown stays at the original hard cap (17:50Z; userdata's +260
# from 13:34Z).  When runner2 is done, the +5 goes back if runner.sh is done.
R=/work/out/RESULTS.txt
shutdown -h 17:50
while pgrep -f '^bash /work/runner2.sh' > /dev/null; do
  grep -q '^DONE ' $R && shutdown -h 17:50
  sleep 60
done
grep -q '^DONE ' $R && shutdown -h +5
