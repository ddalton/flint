#!/bin/bash
# c33b on EC2 2026-10-31: the replacement for c33 (i-0ac5876cab9ffd6bd, m7i.16xlarge us-east-1f, reclaimed by AWS at
# 21:53:34Z: "no Spot capacity"), under the same approval (a second 64-vCPU spot instance for the box's queue and the
# tlapm passes, <= ~$5.40): c6i.16xlarge spot us-east-1d, 64 vCPU / 128 GiB, 300 GB gp3; hard cap shutdown -h +360.
# ONE script, no hand-offs, no bare `wait`.  c33's phase A finished before the reclaim (out-c33a/), so this runs the
# inbox only: until the deadline (345 min) or s3://BK/inbox/STOP, each inbox/<name>.tgz whose sha256 matches
# inbox/<name>.sha256 is a job (unpacked into /work/jobs/<name>, job.sh run in the background, cap: its file `cap`,
# default 1800 s, clipped to the deadline; OUT=/work/out/jobs/<name>); out/jobs.txt logs it.
BK=$1; W=/work; F=$W/formal; cd $W; mkdir -p out jobs done
export HOME=/root; . /root/.cargo/env; export CARGO_INCREMENTAL=0
MC=/root/.cargo/bin/tla-mc; TL=$W/tlaps/tlapm/bin/tlapm
T0=$(date +%s); DL=$(( T0 + 345*60 ))
( while true; do aws s3 sync $W/out s3://$BK/out --quiet; sleep 120; done ) & SYNC=$!
( while true; do { date -u +%TZ; cut -d' ' -f1 /proc/loadavg; free -g | sed -n 2p; df -h $W | tail -1; echo "tlapm $(pgrep -x tlapm | wc -l) z3 $(pgrep -x z3 | wc -l) checkers $(ps -eo args | grep -c '^/work/cache-')"; } >> out/res.txt; sleep 120; done ) & MON=$!
( while sleep 15; do u=$(df --output=pcent / | tail -1 | tr -dc 0-9); [ "$u" -ge 88 ] || continue
    for p in $(ps -eo pid,args | grep "[/]work/cache-[A-Za-z0-9-]*/bin/" | awk '{print $1}'); do
      echo "$(date -u +%TZ) disk ${u}%: cut pid $p" >> $W/out/diskguard.txt; kill $p; done; sleep 90; done ) & DG=$!
md5() { md5sum < $1 | cut -c1-8; }
bye() { echo "$1 $(date -u +%FT%TZ)" >> out/RESULTS.txt; kill $SYNC $MON $DG 2>/dev/null; aws s3 sync $W/out s3://$BK/out --quiet; shutdown -h +5; exit 0; }
clip() { local c=$1 left=$(( DL - $(date +%s) - 60 )); [ $c -gt $left ] && c=$left; echo $c; }
ok=1
[ "$(sha256sum < $W/tlaps.tgz | cut -d' ' -f1)" = "5eaf3aa705b69250afdd53e073ec145c49dfd771d77784a6315fd7d05bad4e44" ] || { echo "TLAPS SHA256 MISMATCH" >> out/RESULTS.txt; ok=0; }
mkdir -p $W/tlaps && tar -xzf $W/tlaps.tgz -C $W/tlaps
v=$($MC -version 2>&1 | head -1); [ "$v" = "tla-mc 0.2.1" ] || { echo "TLA-MC VERSION [$v] != tla-mc 0.2.1" >> out/RESULTS.txt; ok=0; }
[ -x $TL ] || { echo "NO TLAPM at $TL" >> out/RESULTS.txt; ok=0; }
echo "START $(date -u +%FT%TZ) $(nproc) cpus $(free -g | awk '/Mem:/{print $2}')G $v tlapm $($TL --version 2>&1 | head -1) md5ok=$ok" >> out/RESULTS.txt
[ $ok = 1 ] || bye "FATAL inputs"
cat > $W/summ.py <<"PY"
import sys,re
from collections import Counter
t=open(sys.argv[1]).read()
st={}
for bl in re.findall(r"@!!BEGIN\n(.*?)@!!END", t, re.S):
    d=dict(re.findall(r"@!!(\w+):(.*)", bl))
    if "loc" in d and "status" in d: st[d["loc"]]=d["status"]
c=Counter(st.values())
fl=sorted([l for l,s in st.items() if s not in ("proved","trivial")], key=lambda l:[int(n) for n in l.split(":")[:2]])
m=re.findall(r"All \d+ obligations? proved|\d+/\d+ obligations? failed|tlapm ending abnormally.*|[Ee]rror.*|Fatal.*", t)
print("RESULT", dict(c), "|", "; ".join(sorted(set(m))[:5]))
print("FAILED", " ".join(sorted(set(f.split(":")[0] for f in fl), key=int)))
PY
verdict() { # verdict <rc> <out>: the box's wrun2.sh classification; sets v and got
  got=$(grep -m1 -oE "No error has been found|Invariant [A-Za-z_0-9]+ is violated|Temporal propert[a-z]* [A-Za-z_]* *(is|were) violated|tla-mc: [a-z]+:.*|[Ee]rror:.*|No space left on device" $2 | head -1)
  case "$1:$got" in 0:No*) v=HOLD;; *nvariant*violated*|*emporal*violated*) v=VIOL;; 124:*) v=NOVERDICT;; *) v=ERROR;; esac; }

NA=0
# ---- J: the inbox ----
adone=0
while :; do
  [ $(date +%s) -ge $DL ] && { echo "DEADLINE $(date -u +%TZ)" >> out/RESULTS.txt; break; }
  aws s3 ls s3://$BK/inbox/ 2>/dev/null | awk '{print $4}' > $W/inbox.ls
  grep -qx STOP $W/inbox.ls && { echo "STOP $(date -u +%TZ)" >> out/RESULTS.txt; break; }
  for n in $(grep '\.tgz$' $W/inbox.ls | sed 's/\.tgz$//'); do
    [ -e $W/jobs/$n ] && continue; mkdir -p $W/jobs/$n $W/out/jobs/$n
    aws s3 cp s3://$BK/inbox/$n.sha256 $W/jobs/$n.sha256 --quiet; aws s3 cp s3://$BK/inbox/$n.tgz $W/jobs/$n.tgz --quiet
    [ "$(sha256sum < $W/jobs/$n.tgz | cut -d' ' -f1)" = "$(cut -d' ' -f1 $W/jobs/$n.sha256 2>/dev/null)" ] || { echo "JOB $n SHA MISMATCH $(date -u +%TZ)" >> out/jobs.txt; continue; }
    tar -xzf $W/jobs/$n.tgz -C $W/jobs/$n
    cap=$(clip $(cat $W/jobs/$n/cap 2>/dev/null || echo 1800)); [ $cap -lt 60 ] && { echo "JOB $n SKIPPED (deadline)" >> out/jobs.txt; continue; }
    echo "JOB $n START cap ${cap}s $(date -u +%TZ)" >> out/jobs.txt
    ( cd $W/jobs/$n; timeout $cap env OUT=$W/out/jobs/$n MC=$MC TL=$TL SUMM=$W/summ.py W=$W bash job.sh > $W/out/jobs/$n/job.log 2>&1
      echo "JOB $n DONE rc=$? $(date -u +%TZ)" >> $W/out/jobs.txt; aws s3 sync $W/out s3://$BK/out --quiet ) &
  done
  sleep 20
done
bye "DONE"
