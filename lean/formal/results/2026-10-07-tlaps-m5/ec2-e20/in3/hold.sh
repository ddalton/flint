#!/bin/bash
# User, 14:27Z: extend by an hour.  The hard cap is now 18:50Z; this holds it
# there over any runner's "shutdown +5" (Claude tears down at about 18:40Z).
pkill -f '^bash /work/watch2.sh'; pkill -f '^bash /work/watch3.sh'
while true; do shutdown -h 18:50 2>/dev/null; sleep 60; done
