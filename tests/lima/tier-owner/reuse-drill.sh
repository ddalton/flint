#!/usr/bin/env bash
#
# reuse-drill.sh — B12: a reused bucket prefix must not silently serve
# the previous project's data.
#
# Runs INSIDE the Lima VM (repo visible at the host path):
#
#   limactl shell flint-nfs-client -- sudo \
#     HUB_BIN=<flint-pnfs-mds aarch64 build> bash tests/lima/tier-owner/reuse-drill.sh
#
# A real MinIO, the real hub binary, real startups. Two phases:
#
#   UNENFORCED (the control): no ownerIdentity configured anywhere —
#     the pre-B12 posture, still reachable by config. Hub X claims a
#     prefix; a stranger's object is seeded; hub Y (a DIFFERENT
#     project, same prefix) starts fine and IMPORTS it. This phase
#     watches the vulnerability actually happen, so the enforced
#     phase's refusals cannot pass vacuously — the oracle demonstrably
#     sees an adoption when one occurs.
#
#   ENFORCED: ownerIdentity set (what the operator renders from the
#     CR's uid). First claim stamps the owner object; the same
#     identity over fresh state (the hibernate/resume shape) imports
#     fine; a DIFFERENT identity is refused at startup with nothing
#     imported and no listener bound; adoptData:true takes the prefix
#     over and rewrites the owner; after which the ORIGINAL identity
#     is the one refused.
#
# PASS/FAIL tallied per leg; any FAIL exits non-zero.

set -uo pipefail

HUB_BIN="${HUB_BIN:?set HUB_BIN to the flint-pnfs-mds binary}"
RIG=/var/tmp/ownerrig
BUCKET=flintb12
S3_PORT=19000
NFS_PORT=20499
MC="mc --config-dir $RIG/mc"

PASS=0; FAIL=0
ok()   { PASS=$((PASS+1)); echo "  PASS: $*"; }
bad()  { FAIL=$((FAIL+1)); echo "  FAIL: $*"; }
leg()  { echo; echo "=== LEG $* ==="; }

cleanup() {
  for pf in "$RIG"/*/pid "$RIG"/minio.pid; do
    [ -f "$pf" ] && kill "$(cat "$pf")" 2>/dev/null
  done
}
cleanup
rm -rf "$RIG"
mkdir -p "$RIG/minio-data"
trap cleanup EXIT

# ── tools (pinned install path; downloaded once per VM) ───────────────
for tool in minio mc; do
  if ! command -v $tool >/dev/null 2>&1; then
    case $tool in minio) path=server/minio;; mc) path=client/mc;; esac
    echo "installing $tool (linux-arm64)..."
    curl -fsSL -o /usr/local/bin/$tool \
      "https://dl.min.io/$path/release/linux-arm64/$tool" \
      || { echo "cannot download $tool"; exit 2; }
    chmod +x /usr/local/bin/$tool
  fi
done
command -v mc >/dev/null || { echo "mc missing"; exit 2; }

# ── MinIO up ──────────────────────────────────────────────────────────
env MINIO_ROOT_USER=flint MINIO_ROOT_PASSWORD=flintsecret \
  nohup minio server "$RIG/minio-data" --address 127.0.0.1:$S3_PORT \
  > "$RIG/minio.log" 2>&1 &
echo $! > "$RIG/minio.pid"
for _ in $(seq 1 50); do
  (exec 3<>/dev/tcp/127.0.0.1/$S3_PORT) 2>/dev/null && { exec 3>&-; break; }
  sleep 0.2
done
$MC alias set local http://127.0.0.1:$S3_PORT flint flintsecret >/dev/null \
  || { echo "minio never came up"; tail -5 "$RIG/minio.log"; exit 2; }
$MC mb --quiet local/$BUCKET >/dev/null || { echo "mc mb failed"; exit 2; }

# ── hub lifecycle helpers ─────────────────────────────────────────────
write_cfg() { # $1=dir $2=prefix $3=identity(or "") $4=adopt(or "")
  local extra=""
  [ -n "$3" ] && extra="${extra}    ownerIdentity: \"$3\"
"
  [ "$4" = true ] && extra="${extra}    adoptData: true
"
  cat > "$1/config.yaml" <<EOF
apiVersion: chert.us/v1alpha1
kind: PnfsConfig
mode: standalone
mds:
  bind: { address: "127.0.0.1", port: $NFS_PORT }
  layout: { type: file, stripeSize: 8388608, policy: stripe }
  dataServers: []
  state: { backend: sqlite, config: { path: $1/state/state.db } }
  tier:
    enabled: true
    bucket: $BUCKET
    keyPrefix: "$2"
    endpoint: http://127.0.0.1:$S3_PORT
    tickSecs: 1
    quiesceSecs: 1
    flushFloorSecs: 1
    epochHeartbeatSecs: 1
    epochLeaseMisses: 2
$extra
exports:
  - path: $1/exports
    fsid: 1
    options: [rw, sync, no_subtree_check]
    access: [{ network: 0.0.0.0/0, permissions: rw }]
logging: { level: info, format: text }
monitoring:
  prometheus: { enabled: false, port: 0, path: /metrics }
  health: { enabled: false, port: 0, path: /health }
  metrics: []
EOF
}

start_hub() { # $1=name $2=prefix $3=identity $4=adopt -> sets HUB_DIR
  HUB_DIR=$RIG/$1
  mkdir -p "$HUB_DIR/state" "$HUB_DIR/exports"
  write_cfg "$HUB_DIR" "$2" "$3" "$4"
  env AWS_ACCESS_KEY_ID=flint AWS_SECRET_ACCESS_KEY=flintsecret \
    AWS_REGION=us-east-1 \
    nohup "$HUB_BIN" --config "$HUB_DIR/config.yaml" \
    > "$HUB_DIR/hub.log" 2>&1 &
  echo $! > "$HUB_DIR/pid"
}

wait_listening() { # $1=hub dir; success when the NFS port answers
  for _ in $(seq 1 100); do
    (exec 3<>/dev/tcp/127.0.0.1/$NFS_PORT) 2>/dev/null && { exec 3>&-; return 0; }
    kill -0 "$(cat "$1/pid")" 2>/dev/null || return 1  # died first
    sleep 0.2
  done
  return 1
}

wait_dead() { # $1=hub dir; success when the process has exited
  for _ in $(seq 1 100); do
    kill -0 "$(cat "$1/pid")" 2>/dev/null || return 0
    sleep 0.2
  done
  return 1
}

stop_hub() { # $1=hub dir
  kill "$(cat "$1/pid")" 2>/dev/null
  wait_dead "$1" || kill -9 "$(cat "$1/pid")" 2>/dev/null
  # the listener must actually be gone before the next hub binds
  for _ in $(seq 1 50); do
    (exec 3<>/dev/tcp/127.0.0.1/$NFS_PORT) 2>/dev/null || return 0
    exec 3>&-
    sleep 0.2
  done
}

wait_import() { # $1=hub dir $2=file — the import materializes a stub
  for _ in $(seq 1 150); do
    [ -e "$1/exports/$2" ] && return 0
    sleep 0.2
  done
  return 1
}

# ══ PHASE U — the control: NO identity anywhere (pre-B12 posture) ═════
leg "U1: an identity-less hub claims prefix pU/ and stamps NOTHING"
start_hub xu pU/ "" ""
if wait_listening "$HUB_DIR"; then ok "hub started (gate skipped)"
else bad "hub never listened"; tail -5 "$HUB_DIR/hub.log"; fi
grep -q "prefix-owner gate skipped" "$HUB_DIR/hub.log" \
  && ok "gate logged Unenforced" || bad "no Unenforced log line"
stop_hub "$HUB_DIR"
$MC stat local/$BUCKET/pU/.flint/owner >/dev/null 2>&1 \
  && bad "an owner object appeared without an identity" \
  || ok "no owner object written"

leg "U2: a DIFFERENT project on the same prefix imports the stranger's data"
echo "hello-from-the-previous-project" | $MC pipe local/$BUCKET/pU/hello.txt >/dev/null
start_hub yu pU/ "" ""
if wait_listening "$HUB_DIR" && wait_import "$HUB_DIR" hello.txt; then
  ok "CONTROL: the adoption HAPPENED (hub Y serves X's file) — the oracle sees it"
else
  bad "control hub never imported — the enforced refusals below would be vacuous"
  tail -5 "$HUB_DIR/hub.log"
fi
stop_hub "$HUB_DIR"

# ══ PHASE E — enforced: the operator-rendered identity flow ═══════════
leg "E1: uid-A first-claims prefix pE/"
start_hub a1 pE/ uid-A ""
if wait_listening "$HUB_DIR"; then ok "hub A up"; else bad "hub A never listened"; tail -5 "$HUB_DIR/hub.log"; fi
grep -q "prefix owner stamped (first claim)" "$HUB_DIR/hub.log" \
  && ok "first claim logged" || bad "no first-claim log"
stop_hub "$HUB_DIR"
$MC stat local/$BUCKET/pE/.flint/owner >/dev/null 2>&1 \
  && ok "owner object exists" || bad "owner object missing"

leg "E2: same identity, FRESH state (hibernate/resume) still imports"
echo "hello-from-A" | $MC pipe local/$BUCKET/pE/hello.txt >/dev/null
start_hub a2 pE/ uid-A ""
if wait_listening "$HUB_DIR" && wait_import "$HUB_DIR" hello.txt; then
  ok "uid-A resumed over fresh state and restored its data"
else
  bad "resume import failed"; tail -5 "$HUB_DIR/hub.log"
fi
grep -q "prefix owner verified" "$HUB_DIR/hub.log" \
  && ok "owner verified logged" || bad "no owner-verified log"
stop_hub "$HUB_DIR"

leg "E3: uid-B on the reused prefix is REFUSED at startup"
start_hub b1 pE/ uid-B ""
if wait_dead "$HUB_DIR"; then ok "hub B exited instead of serving"
else bad "hub B is still running"; stop_hub "$HUB_DIR"; fi
grep -q "already belongs to uid-A" "$HUB_DIR/hub.log" \
  && ok "refusal names the real owner" || bad "refusal message missing"
[ -e "$HUB_DIR/exports/hello.txt" ] \
  && bad "hub B imported A's data despite the refusal" \
  || ok "nothing imported"
(exec 3<>/dev/tcp/127.0.0.1/$NFS_PORT) 2>/dev/null \
  && { exec 3>&-; bad "something is listening after the refusal"; } \
  || ok "no listener bound"

leg "E4: adoptData takes the prefix over deliberately"
start_hub b2 pE/ uid-B true
if wait_listening "$HUB_DIR" && wait_import "$HUB_DIR" hello.txt; then
  ok "uid-B adopted the prefix and imported"
else
  bad "adoption failed"; tail -5 "$HUB_DIR/hub.log"
fi
grep -q "ADOPTED from uid-A" "$HUB_DIR/hub.log" \
  && ok "adoption logged with the deposed owner" || bad "no adoption log"
stop_hub "$HUB_DIR"
$MC cat local/$BUCKET/pE/.flint/owner 2>/dev/null | grep -q '"identity":"uid-B"' \
  && ok "owner object rewritten to uid-B" || bad "owner object not rewritten"

leg "E5: the roles reversed — uid-A is now the foreigner"
start_hub a3 pE/ uid-A ""
if wait_dead "$HUB_DIR"; then ok "old owner refused after the adoption"
else bad "old owner still started"; stop_hub "$HUB_DIR"; fi
grep -q "already belongs to uid-B" "$HUB_DIR/hub.log" \
  && ok "refusal names the NEW owner" || bad "reversal message missing"

leg "E6: no panics anywhere"
panics=$(cat "$RIG"/*/hub.log | grep -c "panicked")
[ "$panics" = 0 ] && ok "no panics across 7 hub starts" || bad "$panics panic(s)"

echo
echo "B12-DRILL RESULT: PASS=$PASS FAIL=$FAIL"
[ $FAIL -eq 0 ]
