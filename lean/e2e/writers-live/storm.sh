#!/usr/bin/env bash
# storm.sh — one node's part of a multi-node storm leg on real S3.
#
#   storm.sh <leg> <node-index>
#
# NODES hosts share ONE workspace prefix (storm/<leg>); each runs
# WRITERS_PER_NODE `flint-sync run` writers, each driven by agent.sh in
# MODE (hot|churn|vocab|disjoint). Node 0 also runs the gateway and the UI
# actor (UI=1, hot|churn), writes the start signal once every node has
# checked out, and after every node is done collects the leg and judges it
# with oracle.py (storm_collect.py). Faults: KILLS per node, spread over the
# load; odd kills are container restarts (kill -9 the syncer, restart it on
# the same tree and state directory), even kills are POD REPLACEMENTS (kill
# -9 syncer and agent, the tree and state directory go, a new writer checks
# out into a fresh tree, the agent resumes its journal there).
#
# Environment:
#   BUCKET (required)   BIN (flint-sync)   GATEWAY_BIN (node 0 with UI=1)
#   RIG (this directory)  ROOT (/mnt/nvme/storm)  AWS_REGION (us-west-1)
#   NODES (3)  WRITERS_PER_NODE (2)  MODE (hot)  UI (1)  KILLS (0)
#   FLOOR (5)  LOAD_SECS (300)  IDLE_SECS (30)  GO_WAIT_SECS (900)
#   GO_FOLLOW_SECS (2700: nodes 1.. wait this long for go, which node 0 writes
#   only after judging the previous leg)
#
# S3 layout: s3://$BUCKET/_rig/storm/<leg>/{ready-<n>,go,done-<n>,node-<n>.tgz,verdict.json,collect.tgz}
set -uo pipefail

LEG=${1:?usage: storm.sh <leg> <node-index>}
NODE=${2:?usage: storm.sh <leg> <node-index>}
: "${BUCKET:?BUCKET is required}" "${BIN:?BIN is required}"
HERE=$(cd "$(dirname "$0")" && pwd)
RIG=${RIG:-$HERE}
ROOT=${ROOT:-/mnt/nvme/storm}
AWS_REGION=${AWS_REGION:-us-west-1}
NODES=${NODES:-3}
WRITERS_PER_NODE=${WRITERS_PER_NODE:-2}
MODE=${MODE:-hot}
UI=${UI:-1}
KILLS=${KILLS:-0}
FLOOR=${FLOOR:-5}
LOAD_SECS=${LOAD_SECS:-300}
IDLE_SECS=${IDLE_SECS:-30}
GO_WAIT_SECS=${GO_WAIT_SECS:-900}
GO_FOLLOW_SECS=${GO_FOLLOW_SECS:-2700}
GW_PORT=${GW_PORT:-18092}
GW_TOKEN=${GW_TOKEN:-storm-drill-token-0123456789abcdef}
export AWS_REGION

PFX="storm/$LEG"
S3="s3://$BUCKET/_rig/storm/$LEG"
RUN="$ROOT/$LEG/node-$NODE"
mkdir -p "$RUN"
now_ms() { python3 -c 'import time; print(int(time.time()*1000))'; }
log() { echo "storm[$LEG n$NODE] $(date -u +%H:%M:%S) $*" | tee -a "$RUN/storm.log" >&2; }
phase() { echo "{\"ev\":\"$1\",\"node\":$NODE,\"ts_ms\":$(now_ms)${2:+,$2}}" >> "$RUN/phases.jsonl"; }
s3_exists() { aws s3 ls "$1" >/dev/null 2>&1; }
touch_s3() { printf '%s\n' "$(now_ms)" | aws s3 cp --quiet - "$1"; }

cat > "$RUN/meta.node.json" <<EOF
{"leg":"$LEG","node":$NODE,"nodes":$NODES,"writers_per_node":$WRITERS_PER_NODE,"mode":"$MODE","ui":$UI,
 "kills":$KILLS,"floor":$FLOOR,"load_secs":$LOAD_SECS,"idle_secs":$IDLE_SECS,"prefix":"$PFX",
 "bin_sha256":"$(sha256sum "$BIN" | cut -d' ' -f1)","host":"$(hostname)","started_ms":$(now_ms)}
EOF

agent_name() { echo "n${NODE}w$1"; }

start_writer() { # <i> — `flint-sync run` on $RUN/w<i>/tree, appending to its log
  local i=$1 w="$RUN/w$1"
  mkdir -p "$w/tree"
  ( exec env FLINT_SYNC_BUCKET="$BUCKET" FLINT_SYNC_PREFIX="$PFX" FLINT_SYNC_ROOT="$w/tree" \
      FLINT_SYNC_FLOOR_SECS="$FLOOR" FLINT_SYNC_EVENT_TRACE=1 AWS_REGION="$AWS_REGION" \
      "$BIN" run >> "$w/sync.stdout" 2>> "$w/sync.log" ) &
  echo $! > "$w/sync.pid"
}
wait_checkout() { # <i> <timeout>
  local w="$RUN/w$1" deadline=$(( $(date +%s) + $2 ))
  until [ -f "$w/tree/.flint/capabilities.json" ]; do
    [ "$(date +%s)" -lt "$deadline" ] || { log "writer $1 never checked out: $(tail -2 "$w/sync.log" | tr '\n' ' ')"; return 1; }
    kill -0 "$(cat "$w/sync.pid")" 2>/dev/null || { log "writer $1 exited: $(tail -2 "$w/sync.log" | tr '\n' ' ')"; return 1; }
    sleep 1
  done
}
start_agent() { # <i>
  local i=$1 w="$RUN/w$1"
  mkdir -p "$w/agent"
  ( exec env AGENT_ID="$(agent_name "$i")" AGENT_MODE="$MODE" AGENT_SEED=$((1000 + 10 * NODE + i)) TREE="$w/tree" \
      JOURNAL="$w/agent/journal.jsonl" CONTROL_DIR="$w/agent" FLOOR_SECS="$FLOOR" \
      sh "$RIG/agent.sh" 2>> "$w/agent.log" ) &
  echo $! > "$w/agent.pid"
}

cleanup() {
  for f in "$RUN"/w*/agent.pid "$RUN"/w*/sync.pid "$RUN"/gateway.pid "$RUN"/ui.pid; do
    [ -f "$f" ] && kill -9 "$(cat "$f")" 2>/dev/null || true
  done
}
trap cleanup EXIT

# ── writers, then the start signal ──────────────────────────────────────
log "prefix $PFX, $WRITERS_PER_NODE writers, mode $MODE, ui $UI, kills $KILLS"
for i in $(seq 0 $((WRITERS_PER_NODE - 1))); do start_writer "$i"; done
for i in $(seq 0 $((WRITERS_PER_NODE - 1))); do wait_checkout "$i" 180 || { touch_s3 "$S3/void-$NODE"; exit 1; }; done
touch_s3 "$S3/ready-$NODE"
phase ready
if [ "$NODE" = 0 ]; then
  deadline=$(( $(date +%s) + GO_WAIT_SECS ))
  for n in $(seq 1 $((NODES - 1))); do
    until s3_exists "$S3/ready-$n"; do
      s3_exists "$S3/void-$n" && { log "node $n VOIDED"; touch_s3 "$S3/go-void"; exit 1; }
      [ "$(date +%s)" -lt "$deadline" ] || { log "node $n never ready"; touch_s3 "$S3/go-void"; exit 1; }
      sleep 3
    done
  done
  touch_s3 "$S3/go"
else
  deadline=$(( $(date +%s) + GO_FOLLOW_SECS ))
  until s3_exists "$S3/go"; do
    s3_exists "$S3/go-void" && { log "the leg was voided"; exit 1; }
    [ "$(date +%s)" -lt "$deadline" ] || { log "no go signal"; exit 1; }
    sleep 2
  done
fi
phase load_start

# ── load ────────────────────────────────────────────────────────────────
for i in $(seq 0 $((WRITERS_PER_NODE - 1))); do start_agent "$i"; done
if [ "$NODE" = 0 ] && [ "$UI" = 1 ]; then
  mkdir -p "$RUN/ui"
  ( exec env -u FLINT_LEAN_GW_ENDPOINT AWS_REGION="$AWS_REGION" FLINT_LEAN_GW_LISTEN="127.0.0.1:$GW_PORT" \
      FLINT_LEAN_GW_BUCKET="$BUCKET" FLINT_LEAN_GW_TOKEN="$GW_TOKEN" FLINT_LEAN_GW_WORKSPACES="storm=$PFX" \
      "$GATEWAY_BIN" >> "$RUN/gateway.log" 2>&1 ) &
  echo $! > "$RUN/gateway.pid"
  for _ in $(seq 1 100); do curl -s -o /dev/null "http://127.0.0.1:$GW_PORT/healthz" && break; sleep 0.2; done
  UIM=$MODE; case "$MODE" in hot|churn) ;; *) UIM=hot ;; esac
  ( exec env GATEWAY="http://127.0.0.1:$GW_PORT" WORKSPACE=storm GATEWAY_TOKEN="$GW_TOKEN" UI_MODE="$UIM" \
      UI_SEED=$((7000 + NODE)) JOURNAL="$RUN/ui/journal.jsonl" CONTROL_DIR="$RUN/ui" AGENT_SH="$RIG/agent.sh" \
      sh "$RIG/ui.sh" 2>> "$RUN/ui.log" ) &
  echo $! > "$RUN/ui.pid"
fi

kill_at=()
if [ "$KILLS" -gt 0 ]; then
  for k in $(seq 1 "$KILLS"); do kill_at+=( $(( $(date +%s) + LOAD_SECS * k / (KILLS + 1) )) ); done
fi
k=0
end=$(( $(date +%s) + LOAD_SECS ))
while [ "$(date +%s)" -lt "$end" ]; do
  if [ "$k" -lt "$KILLS" ] && [ "$(date +%s)" -ge "${kill_at[$k]}" ]; then
    i=$(( k % WRITERS_PER_NODE )); w="$RUN/w$i"
    if [ $(( (k + NODE) % 2 )) -eq 0 ]; then
      kind=restart
      kill -9 "$(cat "$w/sync.pid")" 2>/dev/null
      phase fault "\"fault\":\"kill-9-syncer\",\"writer\":\"$(agent_name "$i")\""
      sleep 2; start_writer "$i"
      wait_checkout "$i" 120 || log "restarted writer $i did not come back"
    else
      kind=replace
      kill -9 "$(cat "$w/agent.pid")" "$(cat "$w/sync.pid")" 2>/dev/null
      phase fault "\"fault\":\"pod-replacement\",\"writer\":\"$(agent_name "$i")\""
      mv "$w/tree" "$w/tree.replaced-$k"
      sleep 2; start_writer "$i"
      wait_checkout "$i" 180 || log "replacement writer $i did not check out"
      start_agent "$i"
    fi
    log "fault $((k + 1))/$KILLS: $kind of writer $i"
    k=$((k + 1))
  fi
  for i in $(seq 0 $((WRITERS_PER_NODE - 1))); do
    kill -0 "$(cat "$RUN/w$i/sync.pid")" 2>/dev/null || { log "writer $i died unprovoked: $(tail -3 "$RUN/w$i/sync.log" | tr '\n' ' ')"; phase writer_died "\"writer\":$i"; }
  done
  sleep 2
done

# ── stop the actors, idle, drain ────────────────────────────────────────
for i in $(seq 0 $((WRITERS_PER_NODE - 1))); do touch "$RUN/w$i/agent/stop"; done
[ -d "$RUN/ui" ] && touch "$RUN/ui/stop"
deadline=$(( $(date +%s) + 120 ))
for f in "$RUN"/w*/agent.pid "$RUN"/ui.pid; do
  [ -f "$f" ] || continue
  while kill -0 "$(cat "$f")" 2>/dev/null; do
    [ "$(date +%s)" -lt "$deadline" ] || { kill "$(cat "$f")" 2>/dev/null; break; }
    sleep 1
  done
done
[ -f "$RUN/gateway.pid" ] && kill "$(cat "$RUN/gateway.pid")" 2>/dev/null
phase load_end
sleep "$IDLE_SECS"
phase idle_end
for i in $(seq 0 $((WRITERS_PER_NODE - 1))); do kill -TERM "$(cat "$RUN/w$i/sync.pid")" 2>/dev/null; done
deadline=$(( $(date +%s) + 180 ))
for i in $(seq 0 $((WRITERS_PER_NODE - 1))); do
  while kill -0 "$(cat "$RUN/w$i/sync.pid")" 2>/dev/null; do
    [ "$(date +%s)" -lt "$deadline" ] || { log "writer $i did not drain"; phase undrained "\"writer\":$i"; kill -9 "$(cat "$RUN/w$i/sync.pid")" 2>/dev/null; break; }
    sleep 1
  done
done
phase drained
trap - EXIT

# ── this node's evidence ────────────────────────────────────────────────
for i in $(seq 0 $((WRITERS_PER_NODE - 1))); do
  w="$RUN/w$i"; a="$RUN/evidence/agents/$(agent_name "$i")"; mkdir -p "$a"
  cp "$w/agent/journal.jsonl" "$a/journal.jsonl" 2>/dev/null || : > "$a/journal.jsonl"
  (cd "$w/tree" && find . -type f ! -path './.flint*' ! -name '*.flint-sync-tmp' -print0 | LC_ALL=C sort -z \
     | xargs -0 -r sha256sum | sed 's#  \./#  #') > "$a/tree.sha256"
  for c in conflicts.jsonl conflicts.1.jsonl; do
    [ -f "$w/tree/.flint-sync/$c" ] && cp "$w/tree/.flint-sync/$c" "$a/$c"
  done
  # A replaced pod's records went with its emptyDir, but the copies they name
  # are still in the bucket. O4 accounts records against copies, so the
  # replaced incarnations' records are kept for that accounting (named in
  # replaced-conflicts.txt): losing a diagnostic file is the pod replacement
  # working, not a preserve without a record.
  for old in "$w"/tree.replaced-*; do
    [ -f "$old/.flint-sync/conflicts.jsonl" ] || continue
    cat "$old/.flint-sync/conflicts.jsonl" >> "$a/conflicts.jsonl"
    echo "$old $(wc -l < "$old/.flint-sync/conflicts.jsonl")" >> "$a/replaced-conflicts.txt"
  done
  mkdir -p "$RUN/evidence/traces"
  grep '^{"ts_ms":' "$w/sync.log" > "$RUN/evidence/traces/$(agent_name "$i").jsonl" || true
  cp "$w/sync.log" "$a/sync.log"; cp "$w/agent.log" "$a/agent.log" 2>/dev/null || true
done
if [ -d "$RUN/ui" ]; then
  mkdir -p "$RUN/evidence/agents/ui"; cp "$RUN/ui/journal.jsonl" "$RUN/evidence/agents/ui/journal.jsonl" 2>/dev/null || true
  cp "$RUN/gateway.log" "$RUN/ui.log" "$RUN/evidence/" 2>/dev/null || true
fi
cp "$RUN/phases.jsonl" "$RUN/meta.node.json" "$RUN/storm.log" "$RUN/evidence/"
tar -czf "$ROOT/$LEG-node-$NODE.tgz" -C "$RUN" evidence
aws s3 cp --quiet "$ROOT/$LEG-node-$NODE.tgz" "$S3/node-$NODE.tgz"
touch_s3 "$S3/done-$NODE"
log "node done"

# ── node 0: collect and judge the whole leg ─────────────────────────────
if [ "$NODE" = 0 ]; then
  deadline=$(( $(date +%s) + 900 ))
  for n in $(seq 1 $((NODES - 1))); do
    until s3_exists "$S3/done-$n"; do
      [ "$(date +%s)" -lt "$deadline" ] || { log "node $n never finished"; break; }
      sleep 5
    done
  done
  python3 "$RIG/storm_collect.py" "$LEG" "$ROOT/$LEG/collect" 2>&1 | tee -a "$RUN/storm.log"
  aws s3 cp --quiet "$ROOT/$LEG/collect/verdict.json" "$S3/verdict.json" || true
  tar -czf "$ROOT/$LEG-collect.tgz" -C "$ROOT/$LEG" collect
  aws s3 cp --quiet "$ROOT/$LEG-collect.tgz" "$S3/collect.tgz"
  touch_s3 "$S3/judged"
fi
