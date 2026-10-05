set -e
F=/opt/flint/lean/formal
sed -e 's/^  RetireAge = .*/  RetireAge = FALSE/' $F/LeanP1Holds.cfg > $F/LeanP1NoAge.cfg
sed -e 's/^  RetireAge = .*/  RetireAge = FALSE/' -e 's/^  CollectorSparesCited = .*/  CollectorSparesCited = FALSE/' $F/LeanP1Holds.cfg > $F/LeanP1CollectorGreedyNoAge.cfg
cat > /opt/flint/extra.py <<'PY'
import sys
src = open("/opt/flint/jobs.py").read()
exec(src[:src.index("# 1. The RECORD worlds")])
# Added 2026-10-05 ~18:45Z (user): CollectorGreedy is vacuous under RetireAge=TRUE (the flag is read only
# where RetireAge discards it). The rule's real test is with retire age OFF: the baseline, then sparing off.
run("LeanP1NoAge", LF, "LeanP1.tla", "HOLDS", 3600)
run("LeanP1CollectorGreedyNoAge", LF, "LeanP1.tla", "RECORD", 3600)
log(f"EXTRADONE {time.strftime('%FT%TZ', time.gmtime())}")
PY
grep -q extra.py /opt/flint/tlcscale.sh || sed -i '/^set -u; DL=\$1/a python3 /opt/flint/extra.py $DL >> /data/out/extra.log 2>&1' /opt/flint/tlcscale.sh
diff <(sed -n 1,200p $F/LeanP1Holds.cfg) $F/LeanP1CollectorGreedyNoAge.cfg || true
grep -n "extra.py" /opt/flint/tlcscale.sh
echo "LeanP1CollectorGreedy                STOPPED-BY-HAND  identical to LeanP1Holds through depth 28 (595,889,257 gen / 171,949,339 distinct at d19; 544,004,332 at d23): CollectorSparesCited is read only in Collect, where RetireAge=TRUE discards it; vacuous on the shipped shape. Replaced by LeanP1NoAge + LeanP1CollectorGreedyNoAge (after the jobs)." >> /data/out/RESULTS.txt
pkill -f "/data/gt/LeanP1CollectorGreedy/release" && echo killed-collectorgreedy
