#!/bin/bash
# c27 on EC2 2026-10-30 (the user: "write the plan with the spot cost", then the go): m7i.48xlarge spot,
# us-west-1, 192 vCPU / 768 GiB, 2 TB gp3.  ONE script, no hand-offs, no bare `wait`; userdata's
# `shutdown -h +720` is the hard cap, and this script shuts down itself 10 min after its final sync.
#  T. (background) the TLAPS record of FlintCatchupTProof (fresh cache, stretch 4), then its 13
#     controls seeded from the record's cache, 6 at a time: out/recT.txt, ends RECTDONE.
#  M. (foreground, one arm at a time, 160 workers, tla-mc 0.2.1 compiled): the identity first, then
#     rev 12's full rule set at THREE replicas (zombie, lost mark, crash), c16 (flint-cb's rev 7 asks,
#     FlintCatchupT rev 15 + TieNA), the expiry form, SoleOnly, the barrier, FOUR replicas.  Each arm's
#     cap is clipped to the time left before the global deadline; an arm with < 10 min left is SKIPPED.
#     out/c27.txt and out/c16.txt (the batch formats of the box), ends C27DONE.
BK=$1; W=/work; S=$W/formal/ms12; T16=$W/formal/mc16; P=$W/proof; cd $W; mkdir -p out
export HOME=/root; . /root/.cargo/env; export CARGO_INCREMENTAL=0
TL=$W/tlaps/tlapm/bin/tlapm; MC=/root/.cargo/bin/tla-mc
T0=$(date +%s); DL=$(( T0 + 690*60 ))          # stop starting work 30 min before the 720-min cap
( while true; do aws s3 sync $W/out s3://$BK/out --quiet --exclude '*meta-*' --exclude '*cache-*'; sleep 300; done ) & SYNC=$!
( while true; do { date -u +%TZ; cut -d' ' -f1 /proc/loadavg; free -g | sed -n 2p; df -h $W | tail -1; echo "tlapm $(pgrep -x tlapm | wc -l) z3 $(pgrep -x z3 | wc -l)"; } >> out/res.txt; sleep 120; done ) & MON=$!
md5() { md5sum < $1 | cut -c1-8; }
ok=1
for f in "$S/FlintCatchupS.tla 8e54566d" "$S/FlintCatchupSReach.tla 2e192540" "$T16/FlintCatchupT.tla 8ac3cf00" "$T16/FlintCatchupTTieNA.tla 780c1009" \
         "$P/FlintCatchupT.tla 8ac3cf00" "$P/FlintCatchupTInd.tla d5039bf8" "$P/FlintCatchupTProof.tla cf71b06a"; do
  set -- $f; m=$(md5 $1); [ "$m" = "$2" ] || { echo "MD5 MISMATCH $1 $m != $2" >> out/RESULTS.txt; ok=0; }
done
echo "START $(date -u +%FT%TZ) $(nproc) cpus $(free -g | awk '/Mem:/{print $2}')G tla-mc $($MC -version 2>&1 | head -1) md5ok=$ok" >> out/RESULTS.txt
[ $ok = 1 ] || { aws s3 sync $W/out s3://$BK/out --quiet; kill $SYNC $MON; shutdown -h +10; exit 1; }

# ---- T: the TLAPS record and its controls (background; writes out/RECT.done when finished) ----
(
  O=$W/out/recT.txt; R=$W/rec; summ() { python3 - "$1" <<"PY"
import sys,re
from collections import Counter
t=open(sys.argv[1]).read()
st={}
for bl in re.findall(r"@!!BEGIN\n(.*?)@!!END", t, re.S):
    d=dict(re.findall(r"@!!(\w+):(.*)", bl))
    if "loc" in d and "status" in d: st[d["loc"]]=d["status"]
c=Counter(st.values())
fl=sorted([l for l,s in st.items() if s not in ("proved","trivial")], key=lambda l:[int(n) for n in l.split(":")[:2]])
m=re.findall(r"All \d+ obligations? proved|\d+/\d+ obligations? failed|tlapm ending abnormally.*|omitted", t)
print(dict(c), "|", "; ".join(sorted(set(m))), "|", " ".join(sorted(set(f.split(":")[0] for f in fl), key=int)))
PY
  }
  mkdir -p $R; cp $P/FlintCatchupT.tla $P/FlintCatchupTInd.tla $P/FlintCatchupTProof.tla $R/; cd $R
  echo "model $(md5 FlintCatchupT.tla) ind $(md5 FlintCatchupTInd.tla) proof $(md5 FlintCatchupTProof.tla) tlapm bfa9468 start $(date -u +%TZ)" > $O
  t0=$(date +%s)
  /usr/bin/time -v $TL --toolbox 0 0 --cleanfp --stretch 4 --threads 24 FlintCatchupTProof.tla > $W/out/recT-record.out 2> $W/out/recT-record.time
  echo "RECORD rc=$? secs=$(( $(date +%s) - t0 )) threads=24 maxrss=$(grep -oP "Maximum resident set size \(kbytes\): \K\d+" $W/out/recT-record.time)KB $(summ $W/out/recT-record.out)" >> $O
  ctl() { # ctl <name> <file> <edit>: one premise flipped (K-) or one rule removed (M-), cache seeded from the record
    D=$W/ctl-$1; mkdir -p $D; cp $R/FlintCatchupT.tla $R/FlintCatchupTInd.tla $R/FlintCatchupTProof.tla $D/; cp -r $R/.tlacache $D/ 2>/dev/null; cd $D
    python3 - "$2" "$3" <<"PY" || { echo "CTL $1 EDIT FAILED" >> $O; return; }
import sys
p, edit = sys.argv[1], sys.argv[2]
s = open(p).read(); o, n = edit.split("|||")
o = o.replace("<NL>", "\n"); n = n.replace("<NL>", "\n")
if p.endswith("Proof.tla"):
    i = s.index("ASSUME Clean =="); j = s.index("-----", i); blk = s[i:j]
    assert blk.count(o) == 1, (o, blk.count(o)); s = s[:i] + blk.replace(o, n) + s[j:]
else:
    assert s.count(o) == 1, (o, s.count(o)); s = s.replace(o, n)
open(p, "w").write(s)
PY
    t0=$(date +%s); $TL --toolbox 0 0 --stretch 4 --threads 4 FlintCatchupTProof.tla > $W/out/recT-ctl-$1.out 2>&1; rc=$?
    echo "CTL $1 [$(echo "$3" | cut -c1-70)] rc=$rc secs=$(( $(date +%s) - t0 )) $(summ $W/out/recT-ctl-$1.out)" >> $O
  }
  PF=FlintCatchupTProof.tla; MF=FlintCatchupT.tla
  CT=( "K-CutStampsAll|$PF|CutStampsAll = FALSE|||CutStampsAll = TRUE"
       "K-RaidFailsAtDrop|$PF|RaidFailsAtDrop = TRUE|||RaidFailsAtDrop = FALSE"
       "K-Capture|$PF|Capture = TRUE|||Capture = FALSE"
       "K-BaseInclusive|$PF|BaseInclusive = TRUE|||BaseInclusive = FALSE"
       "K-PinRetention|$PF|PinRetention = TRUE|||PinRetention = FALSE"
       "K-Crash|$PF|Crash = FALSE|||Crash = TRUE"
       "K-ZombieMoves|$PF|ZombieMoves = FALSE|||ZombieMoves = TRUE"
       "K-StageOracle|$PF|StageOracle = FALSE|||StageOracle = TRUE"
       "M-SourceTrust|$MF|  /\\ Policy = \"trust\" => t \\in tr[s]<NL>|||"
       "M-CutGone|$MF|  /\\ r \\notin cutGone<NL>|||"
       "M-BaseBelowTgt|$MF|CASE Policy = \"trust\" -> e \\in tr[r] /\\ e <= Tgt|||CASE Policy = \"trust\" -> e \\in tr[r]"
       "M-BulkBelowTgt|$MF|  /\\ Policy = \"trust\" => sb[r] <= Tgt<NL>|||"
       "M-DemoteReverts|$MF|reverted' = IF Policy = \"trust\" THEN [reverted EXCEPT ![r] = -1] ELSE reverted|||reverted' = reverted" )
  i=0
  for e in "${CT[@]}"; do
    n=${e%%|*}; r=${e#*|}; f=${r%%|*}; ed=${r#*|}
    while [ $(pgrep -f "^$TL .*--threads 4" | wc -l) -ge 6 ]; do sleep 20; done   # pgrep -f in a FILE, never in an ssh string
    ( ctl "$n" "$f" "$ed" ) &
    i=$((i+1)); sleep 5
  done
  while [ $(pgrep -f "^$TL .*--threads 4" | wc -l) -gt 0 ]; do sleep 30; done
  echo "RECTDONE $(date -u +%TZ)" >> $O; touch $W/out/RECT.done
) > $W/out/recT-runner.log 2>&1 &

# ---- M: the model-checking arms, one at a time ----
arm() { # arm <batch> <dir> <spec> <cfg> <cap>
  b=$1; d=$2; spec=$3; c=$4; cap=$5; O=$W/out/$b.txt; left=$(( DL - $(date +%s) ))
  [ $left -lt 600 ] && { echo "SKIPPED $c (deadline: ${left}s left)" >> $O; return; }
  [ $cap -gt $left ] && cap=$left
  t0=$(date +%s); cd $d
  ( timeout $cap env TLAMC_CACHE=$W/cache-$b $MC -compile -workers 160 -fpmem 180000 -queue-mem 180000 -checkpoint 0 -metadir $W/meta-$b-$c -config $c.cfg $spec ) > $W/out/$b-$c.out 2>&1
  rc=$?; rm -rf $W/meta-$b-$c
  got=$(grep -m1 -oE "No error has been found|Invariant [A-Za-z_0-9]+ is violated|Temporal propert[a-z]* [A-Za-z_]* *(is|were) violated|tla-mc: [a-z]+:.*|[Ee]rror:.*|No space left on device" $W/out/$b-$c.out | head -1)
  case "$rc:$got" in 0:No*) v=HOLD;; *nvariant*violated*|*emporal*violated*) v=VIOL;; 124:*) v=NOVERDICT;; *) v=ERROR;; esac
  n=${c#FlintCatchup}; [ $b = c16 ] && n=${n#T}     # the box's arm names: XZTNJ, SId12; c16's without the T
  echo "$v $n ($spec) rc=$rc got=[$got] $(grep -oE "[0-9]+ distinct states found\. Depth [0-9]+" $W/out/$b-$c.out | tail -1) $(grep -oE "depth [0-9]+, [0-9]+ generated, [0-9]+ distinct" $W/out/$b-$c.out | tail -1) $(( $(date +%s) - t0 ))s cap=${cap}s" >> $O
  cd $W
}
echo "S $(md5 $S/FlintCatchupS.tla) sreach $(md5 $S/FlintCatchupSReach.tla) tla-mc 0.2.1 m7i.48xlarge workers 160 fpmem/queue-mem 180000 $(date -u +%TZ)" > out/c27.txt
echo "RESUME tla-mc 0.2.1 m7i.48xlarge model $(md5 $T16/FlintCatchupT.tla) tiena $(md5 $T16/FlintCatchupTTieNA.tla) workers 160 $(date -u +%TZ)" > out/c16.txt
SX=FlintCatchupS.tla
arm c27 $S $SX FlintCatchupSId12 900          # the identity: 7,286,102, the box's count, on this machine
arm c27 $S $SX FlintCatchupXZTNJ 10800
arm c27 $S $SX FlintCatchupXLTNJ 7200
arm c27 $S $SX FlintCatchupXCTNJ 7200
for e in NAZombieTieGuardANLAsm NALostTieANL NACrashTieANL11 DFTrustANL DNTrustANL DFTieANL DNTieANL \
         DFTrustNNL DNTrustNNL DFTieNNL DNTieNNL NAL TFL NAZ TFZ NAC TFC; do
  case $e in NA*) sp=FlintCatchupTTieNA.tla;; *) sp=FlintCatchupT.tla;; esac
  arm c16 $T16 $sp FlintCatchupT$e 1800
done
echo "C16DONE $(date -u +%TZ)" >> out/c16.txt
arm c27 $S $SX FlintCatchupXZTNJRisk 5400
arm c27 $S $SX FlintCatchupXZSole 3600
arm c27 $S $SX FlintCatchupXLSole 3600
arm c27 $S $SX FlintCatchupXAck 3600
arm c27 $S $SX FlintCatchupX4ZTNJ 5400
echo "C27DONE $(date -u +%TZ)" >> out/c27.txt

# ---- the end: the TLAPS job (bounded by the deadline), the final sync, shutdown ----
while [ ! -f $W/out/RECT.done ] && [ $(date +%s) -lt $DL ]; do sleep 30; done
[ -f $W/out/RECT.done ] || { echo "RECT CUT at the deadline $(date -u +%TZ)" >> out/recT.txt; for p in $(pgrep -x tlapm) $(pgrep -x z3); do kill $p; done; }
echo "DONE $(date -u +%FT%TZ)" >> out/RESULTS.txt
kill $SYNC $MON; aws s3 sync $W/out s3://$BK/out --quiet --exclude '*meta-*' --exclude '*cache-*'
shutdown -h +10
