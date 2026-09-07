#!/usr/bin/env bash
# C7 — X14's cheap half and X15's second half: does a server that warmed
# while it waited take over WITHOUT paying for the repository?
#
# The claim under test, from the simplification note's X14 row: "a
# challenger restores only after it claims … every roll is a full
# restore of unavailability". The answer built here is the batch log
# (`log.rs`) plus a warm pass before the claim (`follow.rs`). This drill
# runs it against a real S3 API, with the real binary, its real claim
# loop and its real restore.
#
# ANTI-VACUITY. Four traps, and the shape of the drill is what avoids
# them:
#   1. A repository small enough that both arms restore instantly
#      measures nothing. The drill pushes ~200 MB and REFUSES to report
#      a verdict unless the control arm actually fetched most of it and
#      actually took time doing so.
#   2. "The warm arm was fast" is no evidence unless the warm arm warmed.
#      The drill waits for the challenger's own `warm:` line and calls
#      the run INCONCLUSIVE if it never appears.
#   3. Fast can mean empty. Both successors are CLONED from and checked:
#      the tip, a file's bytes, and `fsck` in the clone.
#   4. The control must be the same binary with the feature off
#      (PREWARM=0, LOG_MAX_ENTRIES=0), not an older build and not a
#      different repository.
#
#   bash forge/e2e/composition/c7-prewarm.sh
set -uo pipefail
cd "$(dirname "$0")/../../.."
export WORK=${WORK:-/tmp/fc-c7}
rm -rf "$WORK"; mkdir -p "$WORK"
source forge/e2e/composition/rig.sh
trap rig_clean EXIT
rig_init || { echo "rig_init failed"; exit 1; }
binary_is_fresh || exit 1

# How much content the arms carry. Big enough that the download is a
# number and not a rounding error; small enough to run on a laptop.
# Bytes AND objects. The bytes are what a cold restore downloads; the
# objects are what the proof walks. A rig of five huge blobs would
# measure the first and silently report the second as free, because
# `fsck --connectivity-only` costs per OBJECT and there would be five of
# them.
CHUNKS=${CHUNKS:-5}
CHUNK_MB=${CHUNK_MB:-25}
CHUNK_FILES=${CHUNK_FILES:-3000}

A_CATCHUP=0
A_CLAIM2SERVE=0
now_ms() { python3 -c 'import time;print(int(time.time()*1000))'; }

# The restore line the successor prints for itself:
#   flint-forge: restored seq 6, 7 pack(s) named, 0 file(s) fetched (0.0 MiB), proof Some(...), 12 ms
restore_field() {  # restore_field <logfile> <files|mib|proof|ms>
  python3 - "$1" "$2" <<'PY'
import re,sys
txt=open(sys.argv[1],errors='replace').read()
m=None
for m in re.finditer(r'restored seq (\d+), (\d+) pack\(s\) named, (\d+) file\(s\) fetched \(([\d.]+) MiB\), proof (.*?), (\d+) ms', txt):
    pass
if not m:
    print(''); sys.exit(0)
print({'seq':m.group(1),'packs':m.group(2),'files':m.group(3),'mib':m.group(4),
       'proof':m.group(5),'ms':m.group(6)}[sys.argv[2]])
PY
}

wait_line() {  # wait_line <logfile> <grep-pattern> <secs>
  local f=$1 pat=$2 n=${3:-60}
  for _ in $(seq 1 "$n"); do grep -qE "$pat" "$f" 2>/dev/null && return 0; sleep 1; done
  return 1
}

# ── one arm ──────────────────────────────────────────────────────────
# Returns through the globals: A_FILES A_MIB A_PROOF A_MS A_WAKE A_TIP
run_arm() {  # run_arm <arm> <prewarm 0|1> <log-entries> <status-port>
  local arm=$1 prewarm=$2 logmax=$3 port=$4
  local pfx="tenant/c7-$arm"
  head_ "$arm — prewarm=$prewarm log_max_entries=$logmax"
  rig_purge "$pfx/"

  # The holder. Folds off in BOTH arms: the arms must differ only in
  # the dimension under test, and a base rebuild mid-run would move the
  # pack set under one of them.
  new_bare_repo "$WORK/$arm-a.git"
  forge_up "$arm-a" "$WORK/$arm-a.git" "$pfx" \
    "FLINT_FORGE_STATUS_ADDR=127.0.0.1:$((port+100))" \
    "FLINT_FORGE_LOG_MAX_ENTRIES=$logmax" \
    "FLINT_FORGE_FOLD_FACTOR=0" \
    "FLINT_FORGE_REPACK_THRESHOLD=100000"
  wait_key "$pfx/git/epoch" 30 >/dev/null || { inconc "$arm: the holder never claimed"; return 1; }

  # The content. Random bytes, so git's compression does not turn
  # 200 MB of pushes into 2 MB in the bucket.
  new_clone "$WORK/$arm-a.git" "$WORK/$arm-wc"
  local wc="$WORK/$arm-wc" parent="" push_ms=0 t
  local i
  for i in $(seq 1 "$CHUNKS"); do
    head -c $((CHUNK_MB*1000*1000)) /dev/urandom > "$wc/blob-$i.bin"
    python3 "$REPO_ROOT/forge/e2e/composition/mkfiles.py" "$wc/small-$i" "$CHUNK_FILES"
    git_c "$wc" add "blob-$i.bin" "small-$i" >/dev/null
    git_c "$wc" commit -qm "chunk $i"
    t=$(now_ms)
    FORGE_SOCKET=/tmp/fc-$arm-a.sock push "$wc" HEAD:refs/heads/main > "$WORK/$arm-push$i.log" 2>&1
    push_ms=$((push_ms + $(now_ms) - t))
    grep -q "remote rejected\|error:" "$WORK/$arm-push$i.log" && {
      inconc "$arm: push $i did not land"; return 1; }
  done
  A_TIP=$(git_c "$wc" rev-parse HEAD)
  A_PUSHMS=$((push_ms / CHUNKS))
  local bucket_mb
  bucket_mb=$(aws s3 ls "s3://$BUCKET/$pfx/git/objects/pack/" --summarize 2>/dev/null \
                | awk '/Total Size/{printf "%.0f", $3/1000000}')
  local objects; objects=$(git_c "$WORK/$arm-a.git" rev-list --objects --all 2>/dev/null | wc -l | tr -d ' ')
  note "$arm: $CHUNKS pushes, tip $A_TIP, ${bucket_mb:-?} MB of packs, $objects objects, mean push ${A_PUSHMS} ms"
  if [ "${bucket_mb:-0}" -lt $((CHUNKS*CHUNK_MB/2)) ]; then
    inconc "$arm: the bucket holds ${bucket_mb} MB — too little for a restore to be a number"
    return 1
  fi

  # The challenger, on its own disk, while the holder still holds.
  new_bare_repo "$WORK/$arm-b.git"
  forge_up "$arm-b" "$WORK/$arm-b.git" "$pfx" \
    "FLINT_FORGE_STATUS_ADDR=127.0.0.1:$port" \
    "FLINT_FORGE_HEARTBEAT_SECS=1" \
    "FLINT_FORGE_LOG_MAX_ENTRIES=$logmax" \
    "FLINT_FORGE_PREWARM=$prewarm" \
    "FLINT_FORGE_FOLD_FACTOR=0" \
    "FLINT_FORGE_REPACK_THRESHOLD=100000"
  wait_line "$WORK/forge-$arm-b.log" "another server holds" 30 \
    || { inconc "$arm: the challenger never saw the holder"; return 1; }
  if [ "$prewarm" = "1" ]; then
    wait_line "$WORK/forge-$arm-b.log" "warm: seq .* via the snapshot" 300 \
      || { inconc "$arm: the challenger never warmed — nothing to measure"; return 1; }
  else
    grep -q "flint-forge: warm:" "$WORK/forge-$arm-b.log" \
      && { bad "$arm: PREWARM=0 warmed anyway"; return 1; }
  fi

  # ONE more push after the challenger is warm. This is the case the
  # log exists for and the one a rolling update actually meets: the
  # standby is not caught up at the moment it is needed, it is a few
  # batches behind. Both arms take it, so they still differ only in the
  # dimension under test.
  head -c $((CHUNK_MB*1000*1000)) /dev/urandom > "$wc/blob-late.bin"
  python3 "$REPO_ROOT/forge/e2e/composition/mkfiles.py" "$wc/small-late" "$CHUNK_FILES"
  git_c "$wc" add "blob-late.bin" "small-late" >/dev/null
  git_c "$wc" commit -qm "the push the standby missed"
  FORGE_SOCKET=/tmp/fc-$arm-a.sock push "$wc" HEAD:refs/heads/main > "$WORK/$arm-pushlate.log" 2>&1
  grep -q "remote rejected\|error:" "$WORK/$arm-pushlate.log" && {
    inconc "$arm: the late push did not land"; return 1; }
  A_TIP=$(git_c "$wc" rev-parse HEAD)
  if [ "$prewarm" = "1" ]; then
    wait_line "$WORK/forge-$arm-b.log" "warm: seq .* via 1 log entrie" 120 \
      || { inconc "$arm: the challenger never caught up on the log"; return 1; }
    A_CATCHUP=$(grep -c 'via 1 log entrie' "$WORK/forge-$arm-b.log")
    note "$arm: warm passes: $(grep -c 'flint-forge: warm:' "$WORK/forge-$arm-b.log"), of them $A_CATCHUP through the log"
  fi
  # Let whichever arm it is reach a quiet steady state, so the takeover
  # is not racing a fetch that happens to still be running.
  sleep 5

  # The wake: the holder leaves cleanly (a rollout), and the clock runs
  # until the successor says it is serving.
  python3 "$REPO_ROOT/forge/e2e/composition/watchphase.py" "$port" 300 > "$WORK/$arm-watch.txt" 2>&1 &
  local wpid=$!
  forge_down "$arm-a"
  wait "$wpid"
  local imp srv
  read -r imp srv < "$WORK/$arm-watch.txt"
  case "${imp:-x}${srv:-x}" in
    *[!0-9-]*) inconc "$arm: the phase watcher said: $(cat "$WORK/$arm-watch.txt")"; return 1;;
  esac
  if [ "${srv:--1}" -lt 0 ]; then
    inconc "$arm: the successor never reported serving"; return 1
  fi
  A_WAKE=$srv
  A_CLAIM2SERVE=$((srv - imp))
  sleep 1
  A_FILES=$(restore_field "$WORK/forge-$arm-b.log" files)
  A_MIB=$(restore_field "$WORK/forge-$arm-b.log" mib)
  A_PROOF=$(restore_field "$WORK/forge-$arm-b.log" proof)
  A_MS=$(restore_field "$WORK/forge-$arm-b.log" ms)
  [ -n "$A_FILES" ] || { inconc "$arm: the successor printed no restore line"; return 1; }
  note "$arm: wake ${A_WAKE} ms (of it ${A_CLAIM2SERVE} ms after the claim); restore ${A_MS} ms, ${A_FILES} file(s), ${A_MIB} MiB, proof $A_PROOF"

  # Trap 3: fast must not mean empty. Clone what the successor now
  # holds and check the tip, the bytes and the connectivity.
  rm -rf "$WORK/$arm-check"
  if git clone -q "$WORK/$arm-b.git" "$WORK/$arm-check" 2>/dev/null; then
    local got; got=$(git_c "$WORK/$arm-check" rev-parse HEAD)
    [ "$got" = "$A_TIP" ] && ok "$arm: the successor serves the holder's tip" \
                          || bad "$arm: the successor serves $got, not $A_TIP"
    [ -s "$WORK/$arm-check/blob-$CHUNKS.bin" ] \
      && ok "$arm: and the objects behind it are really there" \
      || bad "$arm: the tip is there and the blob is not"
    git_c "$WORK/$arm-check" fsck --connectivity-only >/dev/null 2>&1 \
      && ok "$arm: the clone passes fsck" || bad "$arm: the clone fails fsck"
  else
    bad "$arm: the successor's repository could not be cloned"
  fi
  forge_down "$arm-b"
  # The arms run one after the other on one laptop disk, and each holds
  # four copies of its content (two bare repositories, a working clone
  # and the check clone). Dropping this arm's before the next one
  # starts is what lets the rig be big enough for the restore to be a
  # number at all.
  rm -rf "$WORK/$arm-wc" "$WORK/$arm-check" "$WORK/$arm-a.git" "$WORK/$arm-b.git"
  return 0
}

# ── the two arms ─────────────────────────────────────────────────────
run_arm cold 0 0 9890 || { verdict "c7-prewarm"; exit $?; }
COLD_FILES=$A_FILES COLD_MIB=$A_MIB COLD_PROOF=$A_PROOF COLD_MS=$A_MS COLD_WAKE=$A_WAKE COLD_PUSH=$A_PUSHMS COLD_C2S=$A_CLAIM2SERVE
run_arm warm 1 512 9891 || { verdict "c7-prewarm"; exit $?; }
WARM_FILES=$A_FILES WARM_MIB=$A_MIB WARM_PROOF=$A_PROOF WARM_MS=$A_MS WARM_WAKE=$A_WAKE WARM_PUSH=$A_PUSHMS WARM_CATCHUP=${A_CATCHUP:-0} WARM_C2S=$A_CLAIM2SERVE

head_ "the control paid, and paid enough to be a measurement"
[ "$COLD_FILES" -gt 0 ] && ok "cold: the successor fetched $COLD_FILES file(s)" \
                        || bad "cold: the successor fetched nothing — the control is vacuous"
awk "BEGIN{exit !($COLD_MIB > $((CHUNKS*CHUNK_MB/3)))}" \
  && ok "cold: and $COLD_MIB MiB of them" \
  || inconc "cold: only $COLD_MIB MiB — the repository is too small to tell the arms apart"
case "$COLD_PROOF" in *Full*) ok "cold: it proved the whole repository ($COLD_PROOF)";;
                      *) bad "cold: expected a full proof, got $COLD_PROOF";; esac

head_ "the warmed successor paid neither the bytes nor the proof"
[ "$WARM_FILES" = "0" ] && ok "warm: the successor fetched 0 files at the claim" \
                        || bad "warm: the successor still fetched $WARM_FILES file(s), $WARM_MIB MiB"
case "$WARM_PROOF" in *Full*) bad "warm: it still walked the whole repository ($WARM_PROOF)";;
                      "") bad "warm: no proof reported";;
                      *) ok "warm: and walked only the delta ($WARM_PROOF)";; esac
[ "$WARM_MS" -lt "$COLD_MS" ] \
  && ok "warm: restore ${WARM_MS} ms against the control's ${COLD_MS} ms" \
  || bad "warm: restore ${WARM_MS} ms is not below the control's ${COLD_MS} ms"
# The wake carries up to one claim poll (1 s) of quantisation in BOTH
# arms, and on a loopback store that noise is larger than the transfer
# it is meant to measure. So the assertion is on the span the successor
# itself owns — from the moment it claimed (phase `importing`) to the
# moment it serves — sampled at 50 ms, with the poll outside it. The
# whole wake is reported beside it and asserted only past the poll.
[ "$WARM_C2S" -lt "$COLD_C2S" ] \
  && ok "warm: claim to serving ${WARM_C2S} ms against the control's ${COLD_C2S} ms" \
  || bad "warm: claim to serving ${WARM_C2S} ms is not below the control's ${COLD_C2S} ms"
if [ "$WARM_WAKE" -lt $((COLD_WAKE - 1000)) ]; then
  ok "warm: whole wake ${WARM_WAKE} ms against the control's ${COLD_WAKE} ms (past the 1 s poll)"
else
  note "whole wake ${WARM_WAKE} vs ${COLD_WAKE} ms — inside one claim poll; on this store the
      transfer runs at loopback speed, so the wake's difference is the claim-to-serving span above
      and the rig cannot resolve more than that"
fi
head_ "the log carried the standby over the push it missed"
if [ "${WARM_CATCHUP:-0}" -gt 0 ]; then
  ok "warm: $WARM_CATCHUP catch-up pass(es) read a log entry rather than the snapshot"
else
  bad "warm: the standby never caught up through the log"
fi

head_ "the log is in the bucket, and only where it was asked for"
LOGS=$(s3_ls "tenant/c7-warm/git/log/" | grep -c '\.json$')
[ "${LOGS:-0}" -gt 0 ] && ok "warm: $LOGS log entrie(s) under git/log/" \
                       || bad "warm: no log entries were written"
COLDLOGS=$(s3_ls "tenant/c7-cold/git/log/" | grep -c '\.json$')
[ "${COLDLOGS:-0}" = "0" ] && ok "cold: LOG_MAX_ENTRIES=0 wrote none" \
                           || bad "cold: $COLDLOGS entries written with the log off"
if FLINT_FORGE_BUCKET=$BUCKET FLINT_FORGE_PREFIX=tenant/c7-warm FLINT_FORGE_ENDPOINT=$ENDPOINT \
     "$FORGE_BIN" --log-list 5 > "$WORK/log-list.txt" 2>&1; then
  grep -q "refs/heads/main" "$WORK/log-list.txt" \
    && ok "--log-list reads them back and names the ref that moved" \
    || bad "--log-list did not name the ref: $(head -3 "$WORK/log-list.txt")"
else
  bad "--log-list failed: $(head -3 "$WORK/log-list.txt")"
fi

head_ "what the log costs the push it rides beside"
note "mean push: ${WARM_PUSH} ms with the log, ${COLD_PUSH} ms without (one arm each; the request
      count is pinned by the unit test, this is the wall clock)"

printf '\nsummary\n'
printf '  %-6s %-8s %-10s %-11s %-7s %-8s %s\n' arm wake_ms claim2srv restore_ms files MiB proof
printf '  %-6s %-8s %-10s %-11s %-7s %-8s %s\n' cold "$COLD_WAKE" "$COLD_C2S" "$COLD_MS" "$COLD_FILES" "$COLD_MIB" "$COLD_PROOF"
printf '  %-6s %-8s %-10s %-11s %-7s %-8s %s\n' warm "$WARM_WAKE" "$WARM_C2S" "$WARM_MS" "$WARM_FILES" "$WARM_MIB" "$WARM_PROOF"
verdict "c7-prewarm"
