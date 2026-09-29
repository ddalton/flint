#!/bin/bash
# flint-46, 2026-09-29: when gate d's decisive and RECORD worlds are done, keep
# LeanP1DeleteOverrideOff (expected UNDECIDED at its 6 h cap) deferred at its
# checkpoint and let the deep LeanP1Holds (full bounds) start instead.
cd ~/lean-leanp1-2026-09-25d || exit 1
until grep -q '^LeanP1NoConvergence ' RESULTS.txt; do sleep 30; done
# the runner shells first: a runner whose checker dies logs it and deletes
# the world's metadir; with the runner gone, the checkpoint stays
pkill -f 'run-leanp1-small-tlcrs-2[.]sh'
sleep 3
pkill -f 'tlc-rs-92c7a987/.*LeanP1DeleteOverrideOff'
sleep 3
[ -e /mnt/nvme2/tlcrs-leanp1d/LeanP1DeleteOverrideOff/ckpt/meta.txt ] && ck=kept || ck=MISSING
echo "LEANP1DONE $(date -u +%FT%TZ) (flint-46): the decisive and RECORD worlds are done; LeanP1DeleteOverrideOff DEFERRED, checkpoint $ck at /mnt/nvme2/tlcrs-leanp1d/LeanP1DeleteOverrideOff (resume: DEFER= run-leanp1-small-tlcrs-2.sh); the deep LeanP1Holds takes the cores" >> RESULTS.txt
