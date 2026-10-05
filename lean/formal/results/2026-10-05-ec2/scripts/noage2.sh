F=/opt/flint/lean/formal
for w in LeanP1NoAge LeanP1CollectorGreedyNoAge; do grep -v '^INVARIANT Inv_ReaderFetches$' $F/$w.cfg > $F/${w}NoReader.cfg; done
diff $F/LeanP1NoAge.cfg $F/LeanP1NoAgeNoReader.cfg
python3 - <<'PY'
p = "/opt/flint/forge.py"; s = open(p).read()
add = '''# Added ~19:15Z: without RetireAge, Inv_ReaderFetches fails at depth 8 by design (ReaderLoses) and masked
# the collector question in both NoAge worlds; rerun them with every other invariant.
run("LeanP1NoAgeNoReader", LF, "LeanP1.tla", "HOLDS", 3600)
run("LeanP1CollectorGreedyNoAgeNoReader", LF, "LeanP1.tla", "RECORD", 3600)
left = DEADLINE - time.time(); half = max(600, (left - 1800) / 2)
'''
anchor = 'left = DEADLINE - time.time(); half = max(600, (left - 1800) / 2)\n'
assert s.count(anchor) == 1 and "NoAgeNoReader" not in s
s = s.replace(anchor, add, 1); open(p, "w").write(s)
PY
grep -n "run(" /opt/flint/forge.py
echo "LeanP1NoAge / LeanP1CollectorGreedyNoAge: Inv_ReaderFetches fails at depth 8 without RetireAge (by design, = ReaderLoses) and masks the rest; my HOLDS expectation was wrong. Requeued as ...NoReader (every other invariant), before the forge worlds." >> /data/out/RESULTS.txt
