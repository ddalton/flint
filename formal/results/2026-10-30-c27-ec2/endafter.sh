#!/bin/bash
# endafter (10-30 c27, the user's word "go with your recommendation"): run XZTNJ (to the disk guard), XLTNJ,
# XCTNJ, then stop.  When XCTNJ's result line lands: stop run.sh (its process group, so no next arm starts),
# kill any compiled checker, cut the TLAPS job if not done (c16 + recT move to the box), final sync, and the
# instance shuts down (terminate on shutdown).  teardown.sh on the Mac then removes the policy and bucket.
BK=flint-c27-20261030; W=/work; O=$W/out
until grep -qE '^[A-Z]+ XCTNJ ' $O/c27.txt 2>/dev/null; do sleep 15; done
t=$(date -u +%TZ)
rs=$(ps -eo pid,args | grep "[b]ash /work/run.sh" | awk 'NR==1{print $1}')
[ -n "$rs" ] && kill -TERM -$(ps -o pgid= -p $rs | tr -d ' ')
sleep 2
for p in $(ps -eo pid,args | grep -E "[/]work/cache-c[0-9]*/bin/|[t]imeout [0-9]+ env TLAMC" | awk '{print $1}'); do kill $p; done
if [ ! -f $O/RECT.done ]; then
  echo "RECT CUT $t: after XCTNJ at the user's word; the TLAPS record moves to the box" >> $O/recT.txt
  rt=$(ps -eo pid,args | grep "[b]ash /work/recT-ec2.sh" | awk 'NR==1{print $1}')
  [ -n "$rt" ] && pkill -TERM -s $(ps -o sid= -p $rt | tr -d ' ')
fi
echo "CUT $t after XCTNJ at the user's word: c16 + recT move to the box; XZTNJRisk XZSole XLSole XAck X4ZTNJ dropped" >> $O/c27.txt
echo "CUT $t (see c27.txt); DONE $(date -u +%FT%TZ)" >> $O/RESULTS.txt
sleep 10
for i in 1 2 3; do aws s3 sync $O s3://$BK/out --exclude 'c16-*' && break; sleep 20; done
aws s3 cp $O/c27.txt s3://$BK/out/c27.txt && echo "FINALSYNC $(date -u +%TZ)" > $O/FINAL && aws s3 cp $O/FINAL s3://$BK/out/FINAL
shutdown -h +1
