set -u
B=flint-tlc-forge-20261005; F=/opt/flint/formal; T=$F/tlc-rs/target/release/tlc-rs; O=/data/out
export HOME=/root AWS_DEFAULT_REGION=us-west-1; source /root/.cargo/env
aws s3 cp s3://$B/payload/rerun/ $F/ --recursive --quiet
pgrep -f "aws s3 cp /data/out" >/dev/null || (setsid nohup bash -c "while true; do aws s3 cp /data/out/ s3://$B/out/ --recursive --quiet; sleep 60; done" > /dev/null 2>&1 < /dev/null &)
echo "RERUN $(date -u +%FT%TZ): the vacuous reclaim worlds moved to PacksOverlap; CodeOverlap without Inv_NamedIsLanded (it is false for overlapping packs)" >> $O/RESULTS.txt
for spec in KeptSetWhileServing:"Inv_(AckedIsDurable|LandedPackComplete)":1200 KeptSetNoRenew:Inv_NoStragglerLandAfterRestore:1200 ProbeKeptSetCommits:ProbeKeptSetCommits:1200 ProbeKeptSetResidue:ProbeKeptSetDropsResidue:1200 Overlap:HOLDS:3600; do
  n=${spec%%:*}; rest=${spec#*:}; exp=${rest%:*}; cap=${rest##*:}; w=ForgeSyncCode$n
  rm -rf /data/gen/$w /data/gt/$w
  (cd $F && $T -codegen /data/gen/$w -config $w.cfg ForgeSync.tla > $O/$w.build.log 2>&1 && cd /data/gen/$w && CARGO_TARGET_DIR=/data/gt/$w cargo build --release >> $O/$w.build.log 2>&1) || { echo "$w BUILD-FAILED" >> $O/RESULTS.txt; continue; }
  t0=$(date +%s); (cd $F && timeout $cap /data/gt/$w/release/tlcgen-forgesync -workers 192 -checkpoint 0 -metadir /data/st/$w -fpmem 32000 -queue-mem 60000 -config $w.cfg ForgeSync.tla > $O/$w.out 2>&1); rc=$?; rm -rf /data/st/$w
  got=$(grep -oE "Invariant [A-Za-z_]+ is violated|Action property [A-Za-z_]+ is violated|No error has been found" $O/$w.out | head -1)
  v=MISMATCH; [ $rc = 124 ] && v=UNDECIDED-CAP
  [ "$exp" = HOLDS ] && [ "$got" = "No error has been found" ] && v=OK-HOLDS
  [ "$exp" != HOLDS ] && echo "$got" | grep -qE "(Invariant|property) ($exp) is violated" && v=OK-FIRES
  printf "%-36s %-14s exp=%-44s | %-56s | %s | %ss (rerun)\n" $w $v "$exp" "$got" "$(grep -oE '[0-9]+ states generated, [0-9]+ distinct states found. Depth [0-9]+' $O/$w.out | tail -1)" $(( $(date +%s) - t0 )) >> $O/RESULTS.txt
done
echo "RERUNDONE $(date -u +%FT%TZ)" >> $O/RESULTS.txt; aws s3 cp $O/ s3://$B/out/ --recursive --quiet
