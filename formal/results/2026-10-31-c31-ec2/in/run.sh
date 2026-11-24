#!/bin/bash
# c31 on EC2 2026-10-31 (the gate's items 5+6; the user: "go ahead with the EC2 session. Use compiled model with
# tla-mc 0.2.2. Build it first."): m7i.48xlarge spot, us-west-1, 192 vCPU / 768 GiB, 2 TB gp3.  ONE script, no
# hand-offs, no bare `wait`; userdata's `shutdown -h +360` is the hard cap; this script shuts down 5 min after its
# final sync.
#  B. BUILD FIRST: every arm's compiled checker (tla-mc 0.2.2, its own TLAMC_CACHE), then ASSERT that each arm's
#     generated source and its locked dependency set equal the box's (in/gensrc.expect: the box built and ran the
#     same arms with the same tla-mc 0.2.2 on 2026-10-31; the identity arm in full, 7,286,102 / d36).  Any
#     mismatch: nothing runs, the instance shuts down.
#  R. one arm at a time, 190 workers: the identity, XAck / XLSole / XCAck (full passes: XAck and XLSole must count
#     as XLTNJ 1,774,204,193 / d45, XCAck as XCTNJ 2,325,892,812 / d46), then XZTNJRisk / XZSole / X4ZTNJ bounded
#     at 1 h.  Each cap is clipped to the deadline (330 min); an arm with < 10 min left is SKIPPED.  out/c31.txt.
BK=$1; W=/work; S=$W/formal/ms12; cd $W; mkdir -p out
export HOME=/root; . /root/.cargo/env; export CARGO_INCREMENTAL=0
MC=/root/.cargo/bin/tla-mc
T0=$(date +%s); DL=$(( T0 + 330*60 ))
( while true; do aws s3 sync $W/out s3://$BK/out --quiet --exclude '*meta-*' --exclude '*cache-*'; sleep 300; done ) & SYNC=$!
( while true; do { date -u +%TZ; cut -d' ' -f1 /proc/loadavg; free -g | sed -n 2p; df -h $W | tail -1; } >> out/res.txt; sleep 120; done ) & MON=$!
( while sleep 15; do u=$(df --output=pcent / | tail -1 | tr -dc 0-9); [ "$u" -ge 88 ] || continue
    for p in $(ps -eo pid,args | grep "[/]work/cache-[A-Za-z0-9]*/bin/" | awk '{print $1}'); do
      echo "$(date -u +%TZ) disk ${u}%: cut pid $p" >> $W/out/diskguard.txt; kill $p; done; sleep 90; done ) & DG=$!
md5() { md5sum < $1 | cut -c1-8; }
bye() { echo "$1 $(date -u +%FT%TZ)" >> out/RESULTS.txt; kill $SYNC $MON $DG; aws s3 sync $W/out s3://$BK/out --quiet --exclude '*meta-*' --exclude '*cache-*'; shutdown -h +5; exit 0; }
ok=1
for f in "FlintCatchupS.tla 8e54566d" "FlintCatchupSReach.tla 2e192540" "FlintCatchupSId12.cfg d3f1d910" "FlintCatchupXAck.cfg 8693daec" \
         "FlintCatchupXLSole.cfg 130d08a5" "FlintCatchupXCAck.cfg d4fd6287" "FlintCatchupXZTNJRisk.cfg 1529db32" \
         "FlintCatchupXZSole.cfg 26f40765" "FlintCatchupX4ZTNJ.cfg 72ae6c87"; do
  set -- $f; m=$(md5 $S/$1); [ "$m" = "$2" ] || { echo "MD5 MISMATCH $1 $m != $2" >> out/RESULTS.txt; ok=0; }
done
v=$($MC -version 2>&1 | head -1); [ "$v" = "tla-mc 0.2.2" ] || { echo "TLA-MC VERSION [$v] != tla-mc 0.2.2" >> out/RESULTS.txt; ok=0; }
echo "START $(date -u +%FT%TZ) $(nproc) cpus $(free -g | awk '/Mem:/{print $2}')G $v md5ok=$ok" >> out/RESULTS.txt
[ $ok = 1 ] || bye "FATAL inputs"

# ---- B: build first, assert the generated sources ----
fp() { g=$(ls -d $1/gen/*/ 2>/dev/null | head -1); [ -n "$g" ] || { echo "gensrc none lockdeps none"; return; }
  echo "gensrc $(cd $g && find src -type f | sort | xargs md5sum | md5sum | cut -c1-8) lockdeps $(grep -E "^name|^version" $g/Cargo.lock | paste - - | grep -v tlcgen | md5sum | cut -c1-8)"; }
ARMS="SId12 XAck XLSole XCAck XZTNJRisk XZSole X4ZTNJ"
for a in $ARMS; do
  # build, then stop the checker as soon as it has loaded (tla-mc execs the built binary: the pid stays); 600 s at most
  cd $S; env TLAMC_CACHE=$W/cache-$a $MC -compile -workers 4 -fpmem 2048 -queue-mem 2048 -metadir $W/meta-b-$a -config FlintCatchup$a.cfg FlintCatchupS.tla > $W/out/b-$a.out 2>&1 &
  bp=$!; n=0
  # (an exited child stays a zombie until reaped and kill -0 still succeeds on it: poll the log, then `wait $bp`)
  while ! grep -qE "loaded in|building the checker failed|-compile: |[Ee]rror:" $W/out/b-$a.out && [ $n -lt 300 ]; do sleep 2; n=$((n+1)); done
  kill $bp 2>/dev/null; wait $bp 2>/dev/null; cd $W
  rm -rf $W/meta-b-$a
  got="$(fp $W/cache-$a)"; want="$(grep "^$a " $W/gensrc.expect | cut -d' ' -f2-5)"
  [ -n "$(ls $W/cache-$a/bin/ 2>/dev/null)" ] || got="$got NOBINARY"
  echo "BUILD $a $got (want $want) $(grep -o "built in [0-9.]*s" out/b-$a.out)" >> out/RESULTS.txt
  [ "$got" = "$want" ] || ok=0
done
[ $ok = 1 ] || bye "FATAL build: a generated source or its dependency set differs from the box's"

# ---- R: the arms ----
arm() { # arm <cfg-name> <cap>
  c=$1; cap=$2; O=$W/out/c31.txt; left=$(( DL - $(date +%s) ))
  [ $left -lt 600 ] && { echo "SKIPPED $c (deadline: ${left}s left)" >> $O; return; }
  [ $cap -gt $left ] && cap=$left
  t0=$(date +%s); cd $S
  ( timeout $cap env TLAMC_CACHE=$W/cache-$c $MC -compile -workers 190 -fpmem 180000 -queue-mem 180000 -checkpoint 0 -metadir $W/meta-$c -config FlintCatchup$c.cfg FlintCatchupS.tla ) > $W/out/c31-$c.out 2>&1
  rc=$?; rm -rf $W/meta-$c
  got=$(grep -m1 -oE "No error has been found|Invariant [A-Za-z_0-9]+ is violated|Temporal propert[a-z]* [A-Za-z_]* *(is|were) violated|tla-mc: [a-z]+:.*|[Ee]rror:.*|No space left on device" $W/out/c31-$c.out | head -1)
  case "$rc:$got" in 0:No*) v=HOLD;; *nvariant*violated*|*emporal*violated*) v=VIOL;; 124:*) v=NOVERDICT;; *) v=ERROR;; esac
  echo "$v $c (FlintCatchupS.tla) rc=$rc got=[$got] $(grep -oE "[0-9]+ distinct states found\. Depth [0-9]+" $W/out/c31-$c.out | tail -1) $(grep -oE "depth [0-9]+, [0-9]+ generated, [0-9]+ distinct" $W/out/c31-$c.out | tail -1) $(( $(date +%s) - t0 ))s cap=${cap}s" >> $O
  cd $W
}
echo "S $(md5 $S/FlintCatchupS.tla) sreach $(md5 $S/FlintCatchupSReach.tla) tla-mc 0.2.2 compiled m7i.48xlarge workers 190 fpmem/queue-mem 180000 $(date -u +%TZ)" > out/c31.txt
arm SId12 900          # the identity on this machine: 7,286,102
arm XAck 3600; arm XLSole 3600; arm XCAck 3600
arm XZTNJRisk 3600; arm XZSole 3600; arm X4ZTNJ 3600
echo "C31DONE $(date -u +%TZ)" >> out/c31.txt
bye "DONE"
