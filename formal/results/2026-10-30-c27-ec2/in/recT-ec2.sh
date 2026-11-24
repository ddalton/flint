#!/bin/bash
# recT-ec2.sh: the TLAPS part of c27's run.sh, started on its own over SSM after tlaps.tgz was found missing from the
# bucket (the first try ran with no tlapm: rc=127, kept as out/recT-notlapm.txt).  Writes out/recT.txt and, at the end,
# out/RECT.done, which run.sh waits for before its final sync and shutdown.
W=/work; P=$W/proof; TL=$W/tlaps/tlapm/bin/tlapm
export HOME=/root
md5() { md5sum < $1 | cut -c1-8; }
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
