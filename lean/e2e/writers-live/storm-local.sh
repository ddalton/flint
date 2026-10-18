#!/usr/bin/env bash
# storm-local.sh — a multi-writer storm on ONE host, against a MinIO
# container: the run a replay against the model (formal/trace/storm2conf.py)
# is developed on before an S3 leg (storm.sh). No AWS, no cluster.
#
#   storm-local.sh <leg>
#
# WRITERS `flint-sync run` writers share one prefix; each is driven by
# agent.sh in MODE. With UI=1 the gateway and the UI actor (ui.sh) write
# through the same prefix. After LOAD_SECS the actors stop, the writers run
# IDLE_SECS more (their floor drains what the last publishes left owed), and
# are stopped. Everything is kept under $ROOT/<leg>:
#   w<i>/sync.log            the writer's stderr (its event trace, `{"ts_ms":` lines)
#   w<i>/agent/journal.jsonl the agent's journal (README §1; `md5` joins an op to its etag)
#   ui/journal.jsonl         the UI actor's journal (UI=1)
#   meta.json                the leg's parameters
#
# Environment: BIN (flint-sync), GATEWAY_BIN (UI=1), RIG (this directory),
#   ROOT (/mnt/nvme/storm-local), WRITERS (3), MODE (hot), UI (0), FLOOR (5),
#   LOAD_SECS (60), IDLE_SECS (30), MINIO_PORT (19000), KEEP_MINIO (0)
set -uo pipefail

LEG=${1:?usage: storm-local.sh <leg>}
: "${BIN:?BIN is required}"
HERE=$(cd "$(dirname "$0")" && pwd)
RIG=${RIG:-$HERE}
ROOT=${ROOT:-/mnt/nvme/storm-local}
WRITERS=${WRITERS:-3}
MODE=${MODE:-hot}
UI=${UI:-0}
FLOOR=${FLOOR:-5}
LOAD_SECS=${LOAD_SECS:-60}
IDLE_SECS=${IDLE_SECS:-30}
MINIO_PORT=${MINIO_PORT:-19000}
GW_PORT=${GW_PORT:-18093}
GW_TOKEN=${GW_TOKEN:-storm-local-token-0123456789abcdef}
BUCKET=agentws
PFX="storm/$LEG"
RUN="$ROOT/$LEG"
CTR="storm-local-minio-$LEG"

rm -rf "$RUN"; mkdir -p "$RUN"
log() { echo "storm-local[$LEG] $(date -u +%H:%M:%S) $*" | tee -a "$RUN/storm.log" >&2; }

export AWS_ACCESS_KEY_ID=stormlocal AWS_SECRET_ACCESS_KEY=stormlocal-secret-0123 AWS_REGION=us-east-1
ENDPOINT="http://127.0.0.1:$MINIO_PORT"

cleanup() {
  for f in "$RUN"/w*/agent.pid "$RUN"/w*/sync.pid "$RUN"/gateway.pid "$RUN"/ui.pid; do
    [ -f "$f" ] && kill -9 "$(cat "$f")" 2>/dev/null || true
  done
  [ "${KEEP_MINIO:-0}" = 1 ] || docker rm -f "$CTR" >/dev/null 2>&1 || true
}
trap cleanup EXIT

# ── the store ───────────────────────────────────────────────────────────
docker rm -f "$CTR" >/dev/null 2>&1 || true
docker run -d --name "$CTR" -p "127.0.0.1:$MINIO_PORT:9000" \
  -e MINIO_ROOT_USER="$AWS_ACCESS_KEY_ID" -e MINIO_ROOT_PASSWORD="$AWS_SECRET_ACCESS_KEY" \
  cgr.dev/chainguard/minio:latest server /data > "$RUN/minio.id" || { log "minio did not start"; exit 1; }
for _ in $(seq 1 60); do curl -s -o /dev/null "$ENDPOINT/minio/health/live" && break; sleep 0.5; done
docker run --rm --network host -e MC_HOST_l="http://$AWS_ACCESS_KEY_ID:$AWS_SECRET_ACCESS_KEY@127.0.0.1:$MINIO_PORT" \
  cgr.dev/chainguard/minio-client:latest mb "l/$BUCKET" >> "$RUN/storm.log" 2>&1 || { log "no bucket"; exit 1; }

cat > "$RUN/meta.json" <<EOF
{"leg":"$LEG","writers":$WRITERS,"mode":"$MODE","ui":$UI,"floor":$FLOOR,"load_secs":$LOAD_SECS,
 "idle_secs":$IDLE_SECS,"prefix":"$PFX","store":"minio","host":"$(hostname)",
 "bin_sha256":"$(sha256sum "$BIN" | cut -d' ' -f1)","started_ms":$(date +%s%3N)}
EOF

agent_name() { echo "w$1"; }
start_writer() { # <i>
  local w="$RUN/w$1"; mkdir -p "$w/tree"
  ( exec env FLINT_SYNC_BUCKET="$BUCKET" FLINT_SYNC_PREFIX="$PFX" FLINT_SYNC_ROOT="$w/tree" \
      FLINT_SYNC_ENDPOINT="$ENDPOINT" FLINT_SYNC_FLOOR_SECS="$FLOOR" FLINT_SYNC_EVENT_TRACE=1 \
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
  local w="$RUN/w$1"; mkdir -p "$w/agent"
  ( exec env AGENT_ID="$(agent_name "$1")" AGENT_MODE="$MODE" AGENT_SEED=$((1000 + $1)) TREE="$w/tree" \
      JOURNAL="$w/agent/journal.jsonl" CONTROL_DIR="$w/agent" FLOOR_SECS="$FLOOR" \
      sh "$RIG/agent.sh" 2>> "$w/agent.log" ) &
  echo $! > "$w/agent.pid"
}

# ── writers, then the load ──────────────────────────────────────────────
log "prefix $PFX, $WRITERS writers, mode $MODE, ui $UI, load ${LOAD_SECS}s"
# One at a time: the first checkout creates the workspace.
for i in $(seq 0 $((WRITERS - 1))); do start_writer "$i"; wait_checkout "$i" 120 || exit 1; done
for i in $(seq 0 $((WRITERS - 1))); do start_agent "$i"; done
if [ "$UI" = 1 ]; then
  : "${GATEWAY_BIN:?GATEWAY_BIN is required with UI=1}"
  mkdir -p "$RUN/ui"
  ( exec env FLINT_LEAN_GW_ENDPOINT="$ENDPOINT" FLINT_LEAN_GW_LISTEN="127.0.0.1:$GW_PORT" \
      FLINT_LEAN_GW_BUCKET="$BUCKET" FLINT_LEAN_GW_TOKEN="$GW_TOKEN" FLINT_LEAN_GW_WORKSPACES="storm=$PFX" \
      "$GATEWAY_BIN" >> "$RUN/gateway.log" 2>&1 ) &
  echo $! > "$RUN/gateway.pid"
  for _ in $(seq 1 100); do curl -s -o /dev/null "http://127.0.0.1:$GW_PORT/healthz" && break; sleep 0.2; done
  ( exec env GATEWAY="http://127.0.0.1:$GW_PORT" WORKSPACE=storm GATEWAY_TOKEN="$GW_TOKEN" UI_MODE="$MODE" \
      UI_SEED=7000 JOURNAL="$RUN/ui/journal.jsonl" CONTROL_DIR="$RUN/ui" AGENT_SH="$RIG/agent.sh" \
      sh "$RIG/ui.sh" 2>> "$RUN/ui.log" ) &
  echo $! > "$RUN/ui.pid"
fi
end=$(( $(date +%s) + LOAD_SECS ))
while [ "$(date +%s)" -lt "$end" ]; do
  for i in $(seq 0 $((WRITERS - 1))); do
    kill -0 "$(cat "$RUN/w$i/sync.pid")" 2>/dev/null || { log "writer $i DIED: $(tail -3 "$RUN/w$i/sync.log" | tr '\n' ' ')"; exit 1; }
  done
  sleep 2
done

# ── stop the actors, idle, stop the writers ─────────────────────────────
for i in $(seq 0 $((WRITERS - 1))); do touch "$RUN/w$i/agent/stop"; done
[ -d "$RUN/ui" ] && touch "$RUN/ui/stop"
deadline=$(( $(date +%s) + 120 ))
for f in "$RUN"/w*/agent.pid "$RUN"/ui.pid; do
  [ -f "$f" ] || continue
  while kill -0 "$(cat "$f")" 2>/dev/null; do
    [ "$(date +%s)" -lt "$deadline" ] || { log "an actor did not stop: $f"; kill "$(cat "$f")" 2>/dev/null; break; }
    sleep 1
  done
done
[ -f "$RUN/gateway.pid" ] && kill "$(cat "$RUN/gateway.pid")" 2>/dev/null
log "actors stopped; idle ${IDLE_SECS}s"
sleep "$IDLE_SECS"
for i in $(seq 0 $((WRITERS - 1))); do kill -TERM "$(cat "$RUN/w$i/sync.pid")" 2>/dev/null; done
sleep 3
for i in $(seq 0 $((WRITERS - 1))); do
  n=$(grep -c '^{"ts_ms":' "$RUN/w$i/sync.log"); a=$(grep -c '"k":"op"' "$RUN/w$i/agent/journal.jsonl" 2>/dev/null)
  log "w$i: $n trace lines, $a agent ops"
done
log "DONE"
