#!/bin/bash
# switch.sh on c31 (the user, 22:24Z: "Repurpose c31"): after XZSole's verdict line, stop run.sh and its loops (before
# its bye: the boot's shutdown -h +360 stays the hard cap), stop the X4ZTNJ arm it starts, record X4ZTNJ as CUT, sync,
# then start the inbox (run3.sh).
W=/work; O=$W/out/c31.txt
until grep -q "^[A-Z]* XZSole " $O; do sleep 5; done
for p in $(ps -eo pid,args | grep "[b]ash /work/run.sh" | awk '{print $1}'); do kill $p; done
sleep 3
for p in $(ps -eo pid,args | grep "[X]4ZTNJ" | awk '{print $1}'); do kill $p; done
sleep 3; for p in $(ps -eo pid,args | grep "[X]4ZTNJ" | awk '{print $1}'); do kill -9 $p; done
rm -rf $W/meta-X4ZTNJ
echo "CUT X4ZTNJ $(date -u +%TZ): not run (the user, 22:24Z, repurposed c31's cores for the proof's rev 17 checks; X4ZTNJ is rev 12 without the zombie triple at MaxFails 2, superseded)" >> $O
echo "C31DONE $(date -u +%TZ) (X4ZTNJ cut; c31's inbox follows: run3.sh)" >> $O
echo "SWITCH $(date -u +%FT%TZ): run.sh stopped after XZSole; X4ZTNJ cut; run3.sh (inbox) starting" >> $W/out/RESULTS.txt
ps -eo pid,args | grep -c "[b]ash /work/run.sh" | sed 's/^/run.sh processes left: /' >> $W/out/RESULTS.txt
aws s3 sync $W/out s3://flint-c31-20261031/out --quiet
# tlapm for the proof's jobs (the pinned build c27/c33 ran: tlaps.tgz sha256 5eaf3aa7...)
if [ "$(sha256sum < $W/in3/tlaps.tgz | cut -d' ' -f1)" = "5eaf3aa705b69250afdd53e073ec145c49dfd771d77784a6315fd7d05bad4e44" ]; then
  mkdir -p $W/tlaps && tar -xzf $W/in3/tlaps.tgz -C $W/tlaps && echo "TLAPM $($W/tlaps/tlapm/bin/tlapm --version 2>&1 | head -1)" >> $W/out/RESULTS.txt
else echo "TLAPS SHA256 MISMATCH: no tlapm jobs" >> $W/out/RESULTS.txt; fi
cp $W/in3/run3.sh $W/run3.sh
setsid -f bash $W/run3.sh flint-c31-20261031 > $W/out/run3.log 2>&1 < /dev/null
