set -u
O=/data/out; B=flint-tlc-run-20261005
# 1. Stop the old chain WITHOUT letting it reach tlcscale/DONE/shutdown: the runner bash first, then jobs.py, then the checker.
pkill -f "bash /opt/flint/runner.sh" ; sleep 1; pkill -f "python3 /opt/flint/jobs.py"; sleep 1; pkill -f "/data/gt/ForgeSyncKeptSetProbeCovered/release"; sleep 2
echo "ForgeSyncKeptSetProbeCovered         STOPPED-BY-HAND  ~77K distinct/s (perf: 63% in tlc-rs's interpreter inside the compiled checker); rerun after compile support lands; LeanP1 work moved ahead (user)" >> $O/RESULTS.txt
pgrep -af "runner.sh|jobs.py|tlcgen" | cut -c1-100
# The S3 sync loop survives (orphaned subshell); check it:
pgrep -af "aws s3 cp /data/out" | head -2 | cut -c1-80; pgrep -f "sleep 60" >/dev/null && echo sync-loop-alive
# 2. jobs2.py: the LeanP1 work now.
cat > /opt/flint/jobs2.py <<'PY'
import sys
src = open("/opt/flint/jobs.py").read()
exec(src[:src.index("# 1. The RECORD worlds")])
run("LeanP1NoAge", LF, "LeanP1.tla", "HOLDS", 3600)
run("LeanP1CollectorGreedyNoAge", LF, "LeanP1.tla", "RECORD", 3600)
run(size3("L3", "A, B, C", 3, 1, 2, 0), LF, "LeanP1.tla", "HOLDS", 5400)
run(size3("L4", "A, B", 3, 1, 3, 0), LF, "LeanP1.tla", "HOLDS", 1800)
run(size3("L4", "A, B, C", 3, 1, 3, 0), LF, "LeanP1.tla", "HOLDS", 7200)
log(f"LEANDONE {time.strftime('%FT%TZ', time.gmtime())}")
PY
cat > /opt/flint/forge.py <<'PY'
import sys
src = open("/opt/flint/jobs.py").read()
exec(src[:src.index("# 1. The RECORD worlds")])
left = DEADLINE - time.time(); half = max(600, (left - 1800) / 2)
run("ForgeSyncKeptSetProbeCovered", FK, "ForgeSyncKeptSet.tla", "ProbeKeptSetDropsCovered", half)
run("ForgeSyncKeptSetVsOriginal", FK, "ForgeSyncKeptSet.tla", "Inv_(AckedIsDurable|LandedPackComplete)", half)
log(f"FORGEDONE {time.strftime('%FT%TZ', time.gmtime())}")
PY
sed -i '/extra.py/d' /opt/flint/tlcscale.sh
# 3. runner2: lean now; then forge once /data/forge-go exists (a new tlc-rs payload to rebuild with) or 1 h before the deadline with the old one; then the TLC step; then DONE.
cat > /opt/flint/runner2.sh <<'SH'
#!/bin/bash
export HOME=/root AWS_DEFAULT_REGION=us-west-1; source /root/.cargo/env
O=/data/out; B=flint-tlc-run-20261005; DL=1791257218
log() { echo "$(date -u +%FT%TZ) $*" | tee -a $O/runner.log; }
log "runner2: lean work first (reordered by the user's request)"
python3 /opt/flint/jobs2.py $DL >> $O/jobs.log 2>&1; log "lean work done"
while [ ! -f /data/forge-go ] && [ $(( DL - $(date +%s) )) -gt 3600 ]; do aws s3 cp s3://$B/payload/forge-go /data/forge-go --quiet 2>/dev/null; sleep 60; done
if [ -f /data/forge-go ] && aws s3 cp s3://$B/payload/tlc-rs.tgz /data/tlc-rs.tgz --quiet 2>/dev/null; then
  rm -rf /opt/flint/formal/tlc-rs.old; mv /opt/flint/formal/tlc-rs /opt/flint/formal/tlc-rs.old; mkdir -p /opt/flint/formal/tlc-rs && tar -xzf /data/tlc-rs.tgz -C /opt/flint/formal/tlc-rs
  cp -r /opt/flint/formal/tlc-rs.old/target /opt/flint/formal/tlc-rs/ 2>/dev/null
  (cd /opt/flint/formal/tlc-rs && cargo build --release > $O/tlcrs-build2.log 2>&1) && log "tlc-rs rebuilt: $(cat /data/forge-go)" || { log "tlc-rs rebuild FAILED, using the old one"; rm -rf /opt/flint/formal/tlc-rs; mv /opt/flint/formal/tlc-rs.old /opt/flint/formal/tlc-rs; }
  rm -rf /data/gen/ForgeSyncKeptSet* /data/gt/ForgeSyncKeptSet*
else log "no new tlc-rs: forge on the old checker"; fi
python3 /opt/flint/forge.py $DL >> $O/jobs.log 2>&1; log "forge done"
bash /opt/flint/tlcscale.sh $DL > $O/tlcscale.log 2>&1; log "tlc step done"
echo DONE > $O/DONE; aws s3 cp $O/ s3://$B/out/ --recursive --quiet; log DONE
shutdown -h +5
SH
setsid nohup bash /opt/flint/runner2.sh > /var/log/flint-runner2.log 2>&1 < /dev/null &
sleep 3; pgrep -af "runner2|jobs2" | cut -c1-80; tail -2 $O/runner.log
