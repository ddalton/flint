#!/usr/bin/env bash
# Shared rig for the COMPOSITION drills (C1-C5).
#
# These drills test what happens when TWO PRODUCTS meet on one bucket.
# Everything else in forge/e2e/ tests forge against itself; here the
# second party is lean (`flint-sync`) or a bare foreign writer standing
# in for a read-write passthrough mount.
#
# WHY MINIO AND NOT THE MEMORY DOUBLE. The export's barrier is not a
# library call — `export::run_barrier` EXECS the shipped `flint-sync`
# binary (export.rs:254). A second process cannot reach an in-process
# `MemoryStore`, so the whole composition is only observable against a
# real endpoint. MinIO is the cheapest one that implements the three
# primitives the arbitration rests on, and `rig_gate` re-proves that on
# every run rather than trusting it.
#
#   source forge/e2e/composition/rig.sh
#   rig_init && rig_gate
#
# Knobs: MINIO_PORT BUCKET WORK KEEP
set -uo pipefail

MINIO_PORT=${MINIO_PORT:-9100}
MINIO_NAME=${MINIO_NAME:-flint-composition-minio}
BUCKET=${BUCKET:-comp}
ENDPOINT="http://127.0.0.1:${MINIO_PORT}"
WORK=${WORK:-$(mktemp -d)}

REPO_ROOT=${REPO_ROOT:-$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")/../../.." && pwd)}
FORGE_BIN=${FORGE_BIN:-$REPO_ROOT/forge/syncer/target/debug/flint-forge-syncer}
HOOK_BIN=${HOOK_BIN:-$REPO_ROOT/forge/syncer/target/debug/flint-forge-hook}
SYNC_BIN=${SYNC_BIN:-$REPO_ROOT/lean/sidecar/target/debug/flint-sync}

export AWS_ACCESS_KEY_ID=${AWS_ACCESS_KEY_ID:-minioadmin}
export AWS_SECRET_ACCESS_KEY=${AWS_SECRET_ACCESS_KEY:-minioadmin}
export AWS_REGION=${AWS_REGION:-us-east-1}
export AWS_DEFAULT_REGION=$AWS_REGION
export AWS_EC2_METADATA_DISABLED=true
export AWS_ENDPOINT_URL=$ENDPOINT

PASS=0; FAIL=0; KNOWN=0; INCONC=0
say()  { printf '%s\n' "$*"; }
head_() { printf '\n== %s ==\n' "$*"; }
ok()   { PASS=$((PASS+1)); printf '  PASS  %s\n' "$*"; }
bad()  { FAIL=$((FAIL+1)); printf '  FAIL  %s\n' "$*"; }
note() { printf '  ....  %s\n' "$*"; }
# A leg that could not measure what it exists to measure. NOT a pass:
# a verdict that counted only PASS and FAIL would report "0 failed"
# about a rig that never injected the latency, never reached the size
# regime, never had a peak-memory figure — the exact way a measurement
# rig lies (tests/k8s/oci-ab: "INCONCLUSIVE is not PASS"). `verdict`
# exits 2 on any of these.
inconc() { INCONC=$((INCONC+1)); printf '  INCONCLUSIVE  %s\n' "$*"; }

# ── accepted conditions ──────────────────────────────────────────────
# These drills found four things the rule forbids and the system
# permits. Three of them were judged not worth fixing (2026-09-05, the
# owner's call; design doc section 17 records it), and the fourth
# cannot be fixed where it is observed.
#
# WHY THEY ARE NOT JUST LEFT RED. A suite that always reports eight
# failures is a suite nobody reads, and a real regression would sit
# unnoticed among the eight. So an accepted outcome is reported, named
# and counted — but it is not a failure, and the suite exits green when
# only accepted conditions are outstanding.
#
#   A1  forge and lean on one prefix do not arbitrate (C1)
#   A2  a nested export prefix is admitted (C1b)
#   A3  the export can neither see nor repair a foreign change (C3, C5)
#   A4  a reader with no manifest cannot be protected (C4)
KNOWN_IDS=""
accepted() {  # accepted <id> <what was observed>
  KNOWN=$((KNOWN+1)); KNOWN_IDS="$KNOWN_IDS $1"
  printf '  KNOWN %s  %s\n' "$1" "$2"
}

# The counterpart, and the reason this is not merely muting. An
# accepted condition that stops reproducing means the RECORD is wrong —
# somebody fixed it, or the leg stopped testing it. Either way a human
# has to look, so it counts as a failure.
stale() {  # stale <id> <what was expected>
  FAIL=$((FAIL+1))
  printf '  STALE %s  %s — this accepted condition no longer reproduces; update section 17\n' \
    "$1" "$2"
}

verdict() {
  if [ "$KNOWN" -gt 0 ]; then
    printf '\n%s: %d passed, %d failed, %d accepted (%s )\n' \
      "${1:-drill}" "$PASS" "$FAIL" "$KNOWN" "$KNOWN_IDS"
  else
    printf '\n%s: %d passed, %d failed\n' "${1:-drill}" "$PASS" "$FAIL"
  fi
  if [ "$INCONC" -gt 0 ]; then
    printf 'INCONCLUSIVE: %d leg(s) could not be decided — this run is NOT green.\n' "$INCONC"
    return 2
  fi
  [ "$FAIL" -eq 0 ]
}

# ── preconditions ────────────────────────────────────────────────────
# The binary under test is the tree under test.
#
# `cargo build --bins` SILENTLY SKIPS flint-forge-syncer: it carries
# `required-features = ["s3"]`, so a plain build leaves whatever binary
# was there before at exactly the path FORGE_BIN points to. A drill run
# after one reports green about code that is not in the tree. Caught
# once for real while writing the large-repo leg.
binary_is_fresh() {
  head_ "precondition: FORGE_BIN is newer than the sources it claims to be"
  # A deliberate control run measures an OLD binary on purpose. Naming
  # the commit is the price of skipping the check: it goes in the log,
  # so a reader can tell a control run from a stale one — which is the
  # failure this precondition exists to catch.
  if [ -n "${CONTROL_COMMIT:-}" ]; then
    note "control run: FORGE_BIN is deliberately ${CONTROL_COMMIT}, not the tree — the freshness check does not apply"
    return 0
  fi
  local newest
  newest=$(find "$REPO_ROOT/forge/syncer/src" "$REPO_ROOT/crates/flint-store/src" \
             -name '*.rs' -newer "$FORGE_BIN" 2>/dev/null | head -3)
  if [ -n "$newest" ]; then
    bad "FORGE_BIN is older than $(echo "$newest" | wc -l | tr -d ' ')+ source file(s):"
    echo "$newest" | sed 's/^/          /'
    say "        rebuild with:  cargo build --bins --features s3"
    return 1
  fi
  ok "FORGE_BIN is not older than any source under forge/syncer or flint-store"
  return 0
}

# ── infrastructure ───────────────────────────────────────────────────
# Kill stragglers from an interrupted run: a syncer that still holds a
# lease would make the NEXT drill's first claim look like contention,
# which is exactly the observation C1 turns on.
rig_reset() {
  pkill -f flint-forge-syncer 2>/dev/null
  pkill -f "flint-sync" 2>/dev/null
  rm -f /tmp/fc-*.sock
  sleep 1
  return 0
}

rig_init() {
  rig_reset
  for b in "$FORGE_BIN" "$HOOK_BIN" "$SYNC_BIN"; do
    [ -x "$b" ] || { say "missing binary: $b"; return 1; }
  done
  docker version --format '{{.Server.Version}}' >/dev/null 2>&1 || {
    say "docker is not answering"; return 1; }
  if ! docker ps --filter "name=$MINIO_NAME" --format '{{.Names}}' | grep -q "$MINIO_NAME"; then
    docker rm -f "$MINIO_NAME" >/dev/null 2>&1
    docker run -d --name "$MINIO_NAME" -p "${MINIO_PORT}:9000" \
      -e MINIO_ROOT_USER=minioadmin -e MINIO_ROOT_PASSWORD=minioadmin \
      quay.io/minio/minio:latest server /data >/dev/null || return 1
    for _ in $(seq 1 30); do
      aws s3api list-buckets >/dev/null 2>&1 && break; sleep 1
    done
  fi
  aws s3api create-bucket --bucket "$BUCKET" >/dev/null 2>&1
  mkdir -p "$WORK"
  return 0
}

# The rig's own falsifiability leg. Every claim below rests on the
# store refusing a conditional PUT; a store that accepted them all
# would make the drills report corruption that is the RIG's, and a
# store that refused them all would make them report safety that is
# not there. So prove both directions before drilling anything.
rig_gate() {
  head_ "rig gate: the store's conditional PUT is real"
  local d="$WORK/gate"; mkdir -p "$d"
  printf 'one\n' > "$d/1"; printf 'two\n' > "$d/2"
  aws s3api delete-object --bucket "$BUCKET" --key rig/gate >/dev/null 2>&1
  aws s3api put-object --bucket "$BUCKET" --key rig/gate --body "$d/1" \
    --if-none-match '*' >/dev/null 2>&1 \
    && ok "If-None-Match:* admits a fresh key" \
    || bad "If-None-Match:* refused a fresh key"
  aws s3api put-object --bucket "$BUCKET" --key rig/gate --body "$d/2" \
    --if-none-match '*' >/dev/null 2>&1 \
    && bad "If-None-Match:* overwrote an existing key" \
    || ok "If-None-Match:* refuses an existing key"
  local et; et=$(s3_etag rig/gate)
  aws s3api put-object --bucket "$BUCKET" --key rig/gate --body "$d/2" \
    --if-match '"00000000000000000000000000000000"' >/dev/null 2>&1 \
    && bad "If-Match honoured a wrong etag" \
    || ok "If-Match refuses a wrong etag"
  aws s3api put-object --bucket "$BUCKET" --key rig/gate --body "$d/2" \
    --if-match "$et" >/dev/null 2>&1 \
    && ok "If-Match admits the right etag" \
    || bad "If-Match refused the right etag"
  [ "$(s3_cat rig/gate)" = "two" ] \
    && ok "the refused PUTs left the bytes alone" \
    || bad "a refused PUT changed the bytes"
}

# ── store helpers ────────────────────────────────────────────────────
s3_etag() { aws s3api head-object --bucket "$BUCKET" --key "$1" \
              --query ETag --output text 2>/dev/null; }
s3_cat()  { local f="$WORK/.cat.$$"; \
            aws s3api get-object --bucket "$BUCKET" --key "$1" "$f" >/dev/null 2>&1 \
              && cat "$f" && rm -f "$f"; }
s3_put()  { aws s3api put-object --bucket "$BUCKET" --key "$1" --body "$2" >/dev/null 2>&1; }
s3_rm()   { aws s3api delete-object --bucket "$BUCKET" --key "$1" >/dev/null 2>&1; }
s3_has()  { aws s3api head-object --bucket "$BUCKET" --key "$1" >/dev/null 2>&1; }
s3_ls()   { aws s3api list-objects-v2 --bucket "$BUCKET" --prefix "$1" \
              --query 'Contents[].Key' --output text 2>/dev/null | tr '\t' '\n'; }

# Delete everything under the given prefixes.
#
# NOT a tidy-up: without it a drill's `wait_key` can be satisfied by an
# object a PREVIOUS run left behind, and the drill then measures a
# corpse. The suite run that first exposed this had C2 reading a lease
# timestamp from the run before it and reporting a frozen heartbeat
# that had simply never started.
rig_purge() {
  local p
  for p in "$@"; do
    aws s3 rm "s3://$BUCKET/$p" --recursive >/dev/null 2>&1
  done
  return 0
}

wait_key() {  # wait_key <key> <secs>
  local k=$1 n=${2:-60}
  for _ in $(seq 1 "$n"); do s3_has "$k" && return 0; sleep 1; done
  return 1
}

# ── git helpers ──────────────────────────────────────────────────────
new_bare_repo() {  # new_bare_repo <dir>
  local d=$1
  git init --bare -q "$d" 2>/dev/null || return 1
  mkdir -p "$d/hooks-flint"
  ln -sf "$HOOK_BIN" "$d/hooks-flint/proc-receive"
  ln -sf "$HOOK_BIN" "$d/hooks-flint/pre-receive"
}

git_c() { git -C "$1" -c user.name=driller -c user.email=driller@invalid "${@:2}"; }

new_clone() {  # new_clone <bare> <dir>
  git clone -q "$1" "$2" 2>/dev/null
  git -C "$2" config user.name driller
  git -C "$2" config user.email driller@invalid
}

# The hook resolves its socket from ITS OWN environment, and git gives
# it the PUSHER's. In the pod both sides default to <repo>/flint-forge/
# and agree without being told; here the socket is moved to /tmp to
# stay under SUN_LEN, so the pusher has to carry the same value or the
# hook reports "the repository server is not accepting writes".
push() {  # push <clone> <refspec>  -> rc, output on stdout
  REMOTE_USER=${REMOTE_USER:-driller} \
  FLINT_FORGE_SOCKET=${FORGE_SOCKET:-/tmp/fc-A.sock} \
    git -C "$1" push origin "$2" 2>&1
}

# ── process helpers ──────────────────────────────────────────────────
# Each forge server gets its own status port so two can run at once.
forge_up() {  # forge_up <tag> <repo> <prefix> [extra env assignments...]
  local tag=$1 repo=$2 prefix=$3; shift 3
  local log="$WORK/forge-$tag.log"
  ( export FLINT_FORGE_BUCKET="$BUCKET" \
           FLINT_FORGE_PREFIX="$prefix" \
           FLINT_FORGE_REPO="$repo" \
           FLINT_FORGE_ENDPOINT="$ENDPOINT" \
           FLINT_FORGE_HOOKS_PATH="$repo/hooks-flint" \
           FLINT_FORGE_SOCKET="/tmp/fc-$tag.sock" \
           FLINT_FORGE_STATUS_ADDR="127.0.0.1:$((9848 + RANDOM % 900))" \
           FLINT_FORGE_HEARTBEAT_SECS=2 \
           FLINT_FORGE_BATCH_WINDOW_MS=200 \
           FLINT_FORGE_SYNC_BIN="$SYNC_BIN"
    for kv in "$@"; do export "$kv"; done
    exec "$FORGE_BIN" ) > "$log" 2>&1 &
  echo $! > "$WORK/forge-$tag.pid"
}

forge_down() {  # forge_down <tag>
  local p="$WORK/forge-$1.pid"
  [ -f "$p" ] && kill "$(cat "$p")" 2>/dev/null; sleep 1
  [ -f "$p" ] && kill -9 "$(cat "$p")" 2>/dev/null; rm -f "$p"
}

forge_log() { cat "$WORK/forge-$1.log" 2>/dev/null; }

lean() {  # lean <subcommand> <prefix> <root> [extra env...]
  local sub=$1 prefix=$2 root=$3; shift 3
  ( export FLINT_SYNC_BUCKET="$BUCKET" \
           FLINT_SYNC_PREFIX="$prefix" \
           FLINT_SYNC_ROOT="$root" \
           FLINT_SYNC_ENDPOINT="$ENDPOINT"
    for kv in "$@"; do export "$kv"; done
    exec "$SYNC_BIN" "$sub" ) 2>&1
}

rig_clean() {
  for p in "$WORK"/forge-*.pid; do [ -f "$p" ] && kill -9 "$(cat "$p")" 2>/dev/null; done
  pkill -f "$SYNC_BIN" 2>/dev/null
  rm -f /tmp/fc-*.sock
  [ "${KEEP:-0}" = "1" ] || rm -rf "$WORK"
}
