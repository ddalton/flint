#!/bin/bash
# After ForgeSyncKeptSetHolds: the two worlds the Mac could not decide in 15 min.
cd ~/forge-keptset-2026-09-29 || exit 1
while pgrep -f '^/bin/bash ./run.sh' >/dev/null; do sleep 60; done
ST=/mnt/nvme2/forge-keptset nice -n 10 ./run.sh ~/lean-gate-2026-09-18/.tla2tools.jar 2 8g 21600 ForgeSyncKeptSetProbeCovered ForgeSyncKeptSetVsOriginal
