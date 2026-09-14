#!/usr/bin/env bash
# contention.sh — the fence-contention leg
# (docs/plans/flint-lean-writer-lease-and-gated-assessment.md §10.2).
#
# N `flint-sync run` writers on ONE host share one workspace prefix, each
# driven by `agent.sh` in disjoint mode (leg A1's load), for LOAD_SECS; the
# agents stop, the writers idle for IDLE_SECS (the idle-tick cost), then
# drain. The oracle: a fresh checkout, every writer's tree digest against
# it, and a HEAD of every citation. Evidence (traces, journals, digests,
# verdict) is tarred and, when BUCKET is set on real S3, copied to
# s3://$BUCKET/_rig/results/.
#
#   contention.sh run <arm> <run-number>
#
# One host is deliberate: the hold and the handoff are S3 round trips, not
# CPU, so writers on one machine contend for the cell as pods on three do,
# and the arms differ in ONE thing — the binary.
#
# Environment:
#   BUCKET        (required)             AWS_REGION   default us-west-1
#   BIN_DIR       holds flint-sync-<arm> (required)
#   RIG           holds agent.sh         default: this script's directory
#   ROOT          run directories        default /tmp/contention
#   WRITERS       default 6              FLOOR        default 5
#   LOAD_SECS     default 300            IDLE_SECS    default 60
#   AWS_PROFILE   passed through (a host with an instance profile needs none)
#   UPLOAD=0      skip the S3 copy of the evidence
set -euo pipefail

HERE=$(cd "$(dirname "$0")" && pwd)
CMD=${1:-}; ARM=${2:-}; N=${3:-}
[ "$CMD" = run ] && [ -n "$ARM" ] && [ -n "$N" ] || { echo "usage: contention.sh run <arm> <n>" >&2; exit 2; }
: "${BUCKET:?BUCKET is required}" "${BIN_DIR:?BIN_DIR is required}"
AWS_REGION=${AWS_REGION:-us-west-1}
RIG=${RIG:-$HERE}
ROOT=${ROOT:-/tmp/contention}
WRITERS=${WRITERS:-6}
FLOOR=${FLOOR:-5}
LOAD_SECS=${LOAD_SECS:-300}
IDLE_SECS=${IDLE_SECS:-60}
UPLOAD=${UPLOAD:-1}
BIN=$BIN_DIR/flint-sync-$ARM
[ -x "$BIN" ] || { echo "contention: no executable $BIN" >&2; exit 2; }

now_ms() { python3 -c 'import time; print(int(time.time()*1000))'; }
sha() { if command -v sha256sum >/dev/null; then sha256sum "$1" | cut -d' ' -f1; else shasum -a 256 "$1" | cut -d' ' -f1; fi; }
log() { echo "contention[$ARM-$N] $(date -u +%H:%M:%S) $*" >&2; }

STAMP=$(date -u +%Y%m%dT%H%M%SZ)
NAME="$ARM-$N-$STAMP"
PFX="contention/$NAME"
RUN="$ROOT/$NAME"
mkdir -p "$RUN"
export AWS_REGION

# EXECs: call it in the background (`&`) or in a subshell, never bare. A
# background FUNCTION is a subshell, and without the exec `$!` names that
# subshell — the SIGTERM below killed it and left the syncer running and
# undrained while the oracle read its tree (the first dry run did exactly
# that, and "passed").
sync_env() { # $1 = tree root, then the command to run in the syncer environment
  local root=$1; shift
  exec env FLINT_SYNC_BUCKET="$BUCKET" FLINT_SYNC_PREFIX="$PFX" FLINT_SYNC_ROOT="$root" \
      FLINT_SYNC_FLOOR_SECS="$FLOOR" FLINT_SYNC_EVENT_TRACE=1 AWS_REGION="$AWS_REGION" "$@"
}

cleanup() {
  for f in "$RUN"/w*/agent.pid "$RUN"/w*/sync.pid; do
    [ -f "$f" ] && kill "$(cat "$f")" 2>/dev/null || true
  done
}
trap cleanup EXIT

cat > "$RUN/meta.json" <<EOF
{"arm": "$ARM", "n": $N, "prefix": "$PFX", "bucket": "$BUCKET", "writers": $WRITERS, "floor": $FLOOR,
 "load_secs": $LOAD_SECS, "idle_secs": $IDLE_SECS, "bin_sha256": "$(sha "$BIN")", "host": "$(hostname)",
 "started_ms": $(now_ms)}
EOF
log "prefix $PFX, $WRITERS writers, floor $FLOOR s, load $LOAD_SECS s, idle $IDLE_SECS s, bin $(sha "$BIN" | cut -c1-12)"

# ── writers ─────────────────────────────────────────────────────────────
for i in $(seq 0 $((WRITERS - 1))); do
  w="$RUN/w$i"; mkdir -p "$w/tree" "$w/agent"
  sync_env "$w/tree" "$BIN" run > "$w/sync.stdout" 2> "$w/sync.log" &
  echo $! > "$w/sync.pid"
done
deadline=$(( $(date +%s) + 180 ))
for i in $(seq 0 $((WRITERS - 1))); do
  until [ -f "$RUN/w$i/tree/.flint/capabilities.json" ]; do
    [ "$(date +%s)" -lt "$deadline" ] || { log "writer $i never came up: $(tail -3 "$RUN/w$i/sync.log")"; echo VOID > "$RUN/verdict.txt"; exit 1; }
    kill -0 "$(cat "$RUN/w$i/sync.pid")" 2>/dev/null || { log "writer $i exited: $(tail -3 "$RUN/w$i/sync.log")"; echo VOID > "$RUN/verdict.txt"; exit 1; }
    sleep 1
  done
done
log "all $WRITERS writers checked out"

# ── load ────────────────────────────────────────────────────────────────
echo "{\"ev\": \"load_start\", \"ts_ms\": $(now_ms)}" >> "$RUN/phases.jsonl"
for i in $(seq 0 $((WRITERS - 1))); do
  w="$RUN/w$i"
  env AGENT_ID="agents-$i" AGENT_MODE=disjoint AGENT_SEED=$((1000 + i)) TREE="$w/tree" \
      JOURNAL="$w/agent/journal.jsonl" CONTROL_DIR="$w/agent" FLOOR_SECS="$FLOOR" \
      sh "$RIG/agent.sh" 2> "$w/agent.log" &
  echo $! > "$w/agent.pid"
done
end=$(( $(date +%s) + LOAD_SECS ))
while [ "$(date +%s)" -lt "$end" ]; do
  for i in $(seq 0 $((WRITERS - 1))); do
    kill -0 "$(cat "$RUN/w$i/sync.pid")" 2>/dev/null || { log "writer $i died under load: $(tail -3 "$RUN/w$i/sync.log")"; echo VOID > "$RUN/verdict.txt"; exit 1; }
  done
  sleep 5
done
for i in $(seq 0 $((WRITERS - 1))); do touch "$RUN/w$i/agent/stop"; done
deadline=$(( $(date +%s) + 90 ))
for i in $(seq 0 $((WRITERS - 1))); do
  while kill -0 "$(cat "$RUN/w$i/agent.pid")" 2>/dev/null; do
    [ "$(date +%s)" -lt "$deadline" ] || { kill "$(cat "$RUN/w$i/agent.pid")" 2>/dev/null || true; break; }
    sleep 1
  done
done
echo "{\"ev\": \"load_end\", \"ts_ms\": $(now_ms)}" >> "$RUN/phases.jsonl"
log "agents stopped; idling $IDLE_SECS s"

# ── idle, then drain ────────────────────────────────────────────────────
sleep "$IDLE_SECS"
echo "{\"ev\": \"idle_end\", \"ts_ms\": $(now_ms)}" >> "$RUN/phases.jsonl"
for i in $(seq 0 $((WRITERS - 1))); do kill -TERM "$(cat "$RUN/w$i/sync.pid")" 2>/dev/null || true; done
deadline=$(( $(date +%s) + 180 ))
for i in $(seq 0 $((WRITERS - 1))); do
  while kill -0 "$(cat "$RUN/w$i/sync.pid")" 2>/dev/null; do
    [ "$(date +%s)" -lt "$deadline" ] || { log "writer $i did not drain"; kill -9 "$(cat "$RUN/w$i/sync.pid")" 2>/dev/null || true; break; }
    sleep 1
  done
done
echo "{\"ev\": \"drained\", \"ts_ms\": $(now_ms)}" >> "$RUN/phases.jsonl"
trap - EXIT
# Nothing of this run may still be running when the oracle reads the trees:
# the recorded pids (the syncers themselves, via the exec) and any process
# of this arm's binary at all (runs are sequential on the host).
survivors=""
for f in "$RUN"/w*/sync.pid "$RUN"/w*/agent.pid; do
  kill -0 "$(cat "$f")" 2>/dev/null && survivors="$survivors $(cat "$f")"
done
survivors="$survivors $(ps -eo pid=,args= | awk -v b="$BIN" 'index($0, b) && !/awk/ {print $1}' | tr '\n' ' ')"
survivors=$(echo $survivors)
if [ -n "$survivors" ]; then
  log "processes of this run survived the drain: $survivors"; echo VOID > "$RUN/verdict.txt"; exit 1
fi
for i in $(seq 0 $((WRITERS - 1))); do
  grep -q '"ev":"barrier_end"' "$RUN/w$i/sync.log" || { log "writer $i ran no barrier"; echo VOID > "$RUN/verdict.txt"; exit 1; }
done

# ── oracle: a fresh checkout, every tree against it, every citation ─────
digest() { # $1 tree -> sorted "path sha" lines, control and state excluded
  (cd "$1" && find . -type f ! -path './.flint/*' ! -path './.flint-sync/*' ! -name '*.flint-sync-tmp' | LC_ALL=C sort | while read -r f; do
    printf '%s %s\n' "${f#./}" "$(sha "$f")"; done)
}
mkdir -p "$RUN/O"
( sync_env "$RUN/O" "$BIN" checkout ) > "$RUN/O.stdout" 2> "$RUN/O.log" || { log "fresh checkout FAILED: $(tail -3 "$RUN/O.log")"; }
digest "$RUN/O" > "$RUN/digest.O.txt"
( sync_env "$RUN/O" "$BIN" manifest ) > "$RUN/manifest.json" 2> "$RUN/manifest.log" || true
bad=0
for i in $(seq 0 $((WRITERS - 1))); do
  digest "$RUN/w$i/tree" > "$RUN/digest.w$i.txt"
  if ! cmp -s "$RUN/digest.O.txt" "$RUN/digest.w$i.txt"; then
    bad=$((bad + 1)); log "writer $i's tree differs from a fresh checkout: $(diff "$RUN/digest.O.txt" "$RUN/digest.w$i.txt" | head -4 | tr '\n' ' ')"
  fi
done
python3 - "$RUN" "$bad" <<'PY'
import json, sys, os
run, bad = sys.argv[1], int(sys.argv[2])
reasons = []
if bad:
    reasons.append(f"{bad} writer tree(s) differ from a fresh checkout")
try:
    m = json.loads(open(os.path.join(run, "manifest.json")).read().strip().splitlines()[-1])
    def norm(e): return (e or "").strip('"')
    gone = [p for p, e in m["entries"].items() if norm((m["heads"].get(p) or {}).get("etag") if isinstance(m["heads"].get(p), dict) else m["heads"].get(p)) != norm(e["etag"] if isinstance(e, dict) else e)]
    if gone:
        reasons.append(f"{len(gone)} citation(s) do not resolve: {gone[:3]}")
    n_entries = len(m["entries"])
except Exception as ex:
    reasons.append(f"manifest verb unreadable: {ex}")
    n_entries = None
if os.path.getsize(os.path.join(run, "digest.O.txt")) == 0:
    reasons.append("the fresh checkout is empty")
verdict = {"pass": not reasons, "reasons": reasons, "entries": n_entries}
open(os.path.join(run, "verdict.json"), "w").write(json.dumps(verdict) + "\n")
print(("PASS" if not reasons else "FAIL") + " " + "; ".join(reasons))
PY

# ── evidence ────────────────────────────────────────────────────────────
tar -czf "$ROOT/$NAME.tgz" -C "$ROOT" --exclude="$NAME/w*/tree" --exclude="$NAME/O" "$NAME"
if [ "$UPLOAD" = 1 ]; then
  aws s3 cp --quiet "$ROOT/$NAME.tgz" "s3://$BUCKET/_rig/results/$NAME.tgz" && log "evidence s3://$BUCKET/_rig/results/$NAME.tgz"
fi
log "done: $NAME"
