#!/usr/bin/env bash
# LATENCY — what a push and a restore cost in round trips, measured.
#
# Every forge drill so far ran against loopback MinIO, where a round
# trip is ~1 ms and the request COUNT is the only thing a leg can see.
# Two changes were shipped on request-count evidence alone:
#
#   * pack siblings uploaded concurrently instead of in series
#     (62775105) — "the concurrency win is structural rather than
#     measured", and stayed that way;
#   * the restore's fan-out across files and chunks (this commit) —
#     restore.rs fetched one file at a time, one chunk at a time, on
#     the path that runs at every pod start.
#
# This leg puts a real round trip in front of the same MinIO
# (toxiproxy, a latency toxic each way) and measures both against the
# ONE control that matters: the same binary with FLINT_FORGE_FANOUT=1,
# which is exactly what the code did before.
#
# PREDICTIONS, STATED BEFORE MEASURING SO THEY CAN BE WRONG.
#   push:    a 1-pack push uploads S siblings (S=3 on git 2.50: .pack,
#            .idx, .rev). Serial: S round trips; fanout 4: 1. Saving
#            = (S-1) x RTT, linear in RTT. Fixed cost unchanged (renew,
#            CAS, two derived files) — so the push costs ~5 RTT after,
#            ~7 before.
#   restore: N single-chunk files. Serial: N round trips; fanout 4:
#            ceil(N/4). Saving = (N - ceil(N/4)) x RTT.
#
# HOUSE RULES (tests/k8s/oci-ab/drive-ab.sh, which earned them):
#   tri-state PASS / FAIL / INCONCLUSIVE — inconclusive is NOT green;
#   arms interleaved with the position CHANGING each rep;
#   arms differ ONLY in the knob (same binary, same bucket, same proxy);
#   a null leg at RTT 0 — the knob alone must move nothing;
#   the rig's own gate first — the proxy must inject what is assumed.
#
#   ./run-latency.sh                       # ~5 min
#   PUSH_RTTS="0 100" RESTORE_RTTS="0 200" ./run-latency.sh
#   KEEP=1 ./run-latency.sh                # keep $WORK (the CSVs)
set -uo pipefail

HERE=$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)
BUCKET=${BUCKET:-latency}
MINIO_NAME=${MINIO_NAME:-flint-latency-minio}
MINIO_PORT=${MINIO_PORT:-9103}
# shellcheck source=../composition/rig.sh
. "$HERE/../composition/rig.sh"

PROXY_PORT=${PROXY_PORT:-9104}
TOXI_PORT=${TOXI_PORT:-8474}
PROXY="http://127.0.0.1:$PROXY_PORT"
TOXI="http://127.0.0.1:$TOXI_PORT"
PUSH_RTTS=${PUSH_RTTS:-"0 50 100 200"}
PUSHES=${PUSHES:-10}                 # measured pushes per arm per RTT
RESTORE_RTTS=${RESTORE_RTTS:-"0 100 200"}
RESTORES=${RESTORES:-5}              # restores per arm per RTT
SEED_PUSHES=${SEED_PUSHES:-10}       # packs in the restored repository, minus one
FANOUT_A=${FANOUT_A:-4}              # arm A: the shipped default
FANOUT_B=1                           # arm B: the control — what the code did before

# Constant across arms. The batch window is 0 so a push's clock does
# not carry 200 ms of deliberate waiting; the heartbeat is slow so a
# renew rarely sits in the serving loop ahead of a push; the repack
# threshold is out of reach so no push pays for a repack.
COMMON="FLINT_FORGE_ENDPOINT=$PROXY FLINT_FORGE_BATCH_WINDOW_MS=0 FLINT_FORGE_HEARTBEAT_SECS=30 FLINT_FORGE_REPACK_THRESHOLD=1000"

now_ms() { perl -MTime::HiRes=time -e 'printf "%.0f\n", time()*1000'; }
ceil_div() { echo $(( ($1 + $2 - 1) / $2 )); }

# ── the proxy ────────────────────────────────────────────────────────
# toxiproxy terminates TCP, so the handshake is local and only DATA
# pays the toxic: one request-response pays one RTT, a connection
# setup pays nothing. Real S3 charges 2-3 RTT per new connection, so
# this UNDERSTATES the cost of every arm equally and the saving not at
# all — the SDK pools connections either way.
proxy_up() {
  toxiproxy-server -host 127.0.0.1 -port "$TOXI_PORT" > "$WORK/toxiproxy.log" 2>&1 &
  echo $! > "$WORK/toxiproxy.pid"
  for _ in $(seq 1 50); do curl -sf "$TOXI/version" >/dev/null 2>&1 && break; sleep 0.1; done
  curl -sf "$TOXI/version" >/dev/null 2>&1 || { say "toxiproxy did not start (see $WORK/toxiproxy.log)"; return 1; }
  toxiproxy-cli -h "$TOXI" create -l "127.0.0.1:$PROXY_PORT" -u "127.0.0.1:$MINIO_PORT" minio >/dev/null || return 1
  toxiproxy-cli -h "$TOXI" toxic add -t latency -n up --upstream -a latency=0 -a jitter=0 minio >/dev/null || return 1
  toxiproxy-cli -h "$TOXI" toxic add -t latency -n down --downstream -a latency=0 -a jitter=0 minio >/dev/null || return 1
  return 0
}
set_rtt() {  # set_rtt <ms> — half each way, so one request-response pays the whole RTT
  local half=$(( $1 / 2 ))
  toxiproxy-cli -h "$TOXI" toxic update -n up -a latency=$half minio >/dev/null
  toxiproxy-cli -h "$TOXI" toxic update -n down -a latency=$half minio >/dev/null
}
proxy_down() {
  [ -f "$WORK/toxiproxy.pid" ] && kill "$(cat "$WORK/toxiproxy.pid")" 2>/dev/null
  rm -f "$WORK/toxiproxy.pid"
}

probe_ms() {  # median of 5 GET / (one request-response each) at an endpoint, in ms
  local ep=$1 i t ts=""
  for i in 1 2 3 4 5; do
    t=$(curl -s -o /dev/null -w '%{time_total}' "$ep/" 2>/dev/null); ts="$ts $t"
  done
  # shellcheck disable=SC2086
  python3 -c "import sys,statistics; print(int(statistics.median(float(x)*1000 for x in sys.argv[1:])))" $ts
}

# P0 — the rig's own falsifiability leg. A proxy that injected nothing
# would make both legs report "no saving" about code that saves; one
# that injected twice what it claims would make the fitted round-trip
# counts nonsense. Prove the assumption before anything rests on it.
proxy_gate() {
  head_ "P0 — the proxy injects the round trip the legs assume"
  set_rtt 0
  local direct via0 via200 added
  direct=$(probe_ms "$ENDPOINT"); via0=$(probe_ms "$PROXY")
  set_rtt 200; via200=$(probe_ms "$PROXY"); set_rtt 0
  note "GET / direct ${direct} ms; via proxy at RTT 0 ${via0} ms; via proxy at RTT 200 ${via200} ms"
  added=$((via200 - via0))
  if [ "$added" -ge 150 ] && [ "$added" -le 300 ]; then
    ok "RTT 200 adds ${added} ms to one request-response"
  else
    inconc "RTT 200 adds ${added} ms, not ~200 — the proxy is not injecting what the legs assume"
    return 1
  fi
  if [ $((via0 - direct)) -le 30 ]; then
    ok "at RTT 0 the proxy itself costs $((via0 - direct)) ms — the null leg is a null"
  else
    inconc "the proxy itself costs $((via0 - direct)) ms at RTT 0"
    return 1
  fi
}

wait_sock() {  # wait_sock <sock> <secs>
  local s=$1 n=$(( ${2:-60} * 10 ))
  for _ in $(seq 1 "$n"); do [ -S "$s" ] && return 0; sleep 0.1; done
  return 1
}

# SIGTERM and WAIT for the exit: the syncer releases its lease on the
# way out (server.rs "lease released; a successor may claim at once").
# rig.sh's forge_down waits one second and then SIGKILLs, and a killed
# holder leaves a successor to wait out the takeover window — which
# would be measured here as a very slow restore.
stop_forge() {
  local p="$WORK/forge-$1.pid" pid
  [ -f "$p" ] || return 0
  pid=$(cat "$p"); kill "$pid" 2>/dev/null
  for _ in $(seq 1 100); do kill -0 "$pid" 2>/dev/null || break; sleep 0.1; done
  kill -9 "$pid" 2>/dev/null; rm -f "$p"
  return 0
}

tally() {  # tally <file of PASS/FAIL/INCONCLUSIVE/NOTE lines>
  local kind rest
  while read -r kind rest; do
    case "$kind" in
      PASS) ok "$rest" ;;
      FAIL) bad "$rest" ;;
      INCONCLUSIVE) inconc "$rest" ;;
      *) note "$rest" ;;
    esac
  done < "$1"
}

# ── P1: the push ─────────────────────────────────────────────────────
SIBLINGS=""
push_leg() {  # push_leg <rtt-ms>
  local r=$1
  head_ "P1 @ RTT ${r} ms — ${PUSHES} pushes per arm, fanout ${FANOUT_A} (A) vs fanout ${FANOUT_B} (B)"
  set_rtt "$r"
  local pa="lat/push-$r/A" pb="lat/push-$r/B"
  rig_purge "$pa" "$pb"
  local ba="$WORK/push-$r-A.git" bb="$WORK/push-$r-B.git" ca="$WORK/push-$r-A" cb="$WORK/push-$r-B"
  new_bare_repo "$ba" && new_bare_repo "$bb" || { bad "could not init the bare repos"; return 1; }
  new_clone "$ba" "$ca"; new_clone "$bb" "$cb"
  # shellcheck disable=SC2086
  forge_up "A$r" "$ba" "$pa" $COMMON FLINT_FORGE_FANOUT=$FANOUT_A
  # shellcheck disable=SC2086
  forge_up "B$r" "$bb" "$pb" $COMMON FLINT_FORGE_FANOUT=$FANOUT_B
  if ! wait_sock "/tmp/fc-A$r.sock" 60 || ! wait_sock "/tmp/fc-B$r.sock" 60; then
    inconc "a syncer never opened its socket at RTT $r"; stop_forge "A$r"; stop_forge "B$r"; return 1
  fi
  if ! python3 "$HERE/pushes.py" --rtt "$r" --pushes "$PUSHES" \
        --arm A "$ca" "/tmp/fc-A$r.sock" --arm B "$cb" "/tmp/fc-B$r.sock" >> "$WORK/pushes.csv"; then
    bad "the push driver failed at RTT $r"; stop_forge "A$r"; stop_forge "B$r"; return 1
  fi
  stop_forge "A$r"; stop_forge "B$r"
  # The workload CAN show a difference only if every push left a pack
  # with siblings: a push git unpacked to loose objects uploads nothing
  # and both arms cost the same. Count what the bucket holds.
  local arm p packs files
  for arm in A B; do
    p="lat/push-$r/$arm"
    packs=$(s3_ls "$p/git/objects/pack/" | grep -c '\.pack$')
    files=$(s3_ls "$p/git/objects/pack/" | grep -c '/pack-')
    if [ "$packs" -ne $((PUSHES + 1)) ]; then
      inconc "arm $arm left $packs packs for $((PUSHES + 1)) pushes — not every push was a pack"
      return 1
    fi
    local s=$((files / packs))
    if [ -n "$SIBLINGS" ] && [ "$s" -ne "$SIBLINGS" ]; then
      inconc "arm $arm at RTT $r has $s siblings per pack, earlier legs had $SIBLINGS"; return 1
    fi
    SIBLINGS=$s
  done
  ok "every push left one pack of ${SIBLINGS} siblings in both arms ($((PUSHES + 1)) packs each)"
}

# ── P2: the restore ──────────────────────────────────────────────────
SEED_PREFIX="lat/restore"
SEED_TIP=""; SEED_FILES=0; SEED_PACKS=0
seed_restore_repo() {
  head_ "P2 seed — a repository of $((SEED_PUSHES + 1)) packs to restore"
  set_rtt 0
  rig_purge "$SEED_PREFIX"
  local bare="$WORK/seed.git" clone="$WORK/seed"
  new_bare_repo "$bare" || { bad "could not init the seed repo"; return 1; }
  new_clone "$bare" "$clone"
  # shellcheck disable=SC2086
  forge_up S "$bare" "$SEED_PREFIX" $COMMON FLINT_FORGE_FANOUT=$FANOUT_A
  wait_sock /tmp/fc-S.sock 60 || { inconc "the seed syncer never opened its socket"; stop_forge S; return 1; }
  if ! python3 "$HERE/pushes.py" --rtt 0 --pushes "$SEED_PUSHES" --arm S "$clone" /tmp/fc-S.sock > /dev/null; then
    bad "seeding pushes failed"; stop_forge S; return 1
  fi
  SEED_TIP=$(git -C "$clone" rev-parse HEAD)
  stop_forge S
  SEED_PACKS=$(s3_ls "$SEED_PREFIX/git/objects/pack/" | grep -c '\.pack$')
  SEED_FILES=$(s3_ls "$SEED_PREFIX/git/objects/pack/" | grep -c '/pack-')
  if [ "$SEED_PACKS" -eq $((SEED_PUSHES + 1)) ] && [ "$SEED_FILES" -gt "$SEED_PACKS" ]; then
    ok "seeded ${SEED_PACKS} packs, ${SEED_FILES} files, tip ${SEED_TIP}"
  else
    inconc "seed left ${SEED_PACKS} packs / ${SEED_FILES} files for $((SEED_PUSHES + 1)) pushes"; return 1
  fi
}

# Process start to socket: claim, snapshot, list, the fetch, HEAD, then
# serve. Everything but the fetch is the same in both arms. The socket
# is the signal for the reason the large-repo leg gives — it opens only
# after restore returns, and there is no exit-after-restore knob.
time_restore() {  # time_restore <tag> <fanout> -> ms on stdout, 1 if it never came up
  # One `local` per line: `local a=$1 b="$a"` expands `$a` BEFORE the
  # builtin assigns it, which under `set -u` is an unbound variable.
  local tag=$1
  local fanout=$2
  local bare="$WORK/restore-$tag.git"
  local sock="/tmp/fc-$tag.sock"
  rm -rf "$bare"; new_bare_repo "$bare" || return 1
  rm -f "$sock"
  local t0 t1
  t0=$(now_ms)
  # shellcheck disable=SC2086
  forge_up "$tag" "$bare" "$SEED_PREFIX" $COMMON FLINT_FORGE_HEARTBEAT_SECS=2 FLINT_FORGE_FANOUT=$fanout
  if ! wait_sock "$sock" 120; then stop_forge "$tag"; return 1; fi
  t1=$(now_ms)
  stop_forge "$tag"
  echo $((t1 - t0))
}

restore_leg() {  # restore_leg <rtt-ms>
  local r=$1
  head_ "P2 @ RTT ${r} ms — ${RESTORES} restores of ${SEED_FILES} files per arm, fanout ${FANOUT_A} (A) vs fanout ${FANOUT_B} (B)"
  set_rtt "$r"
  local rep order arm pos f ms
  for rep in $(seq 1 "$RESTORES"); do
    if [ $((rep % 2)) -eq 1 ]; then order="A B"; else order="B A"; fi
    pos=0
    for arm in $order; do
      f=$FANOUT_A; [ "$arm" = B ] && f=$FANOUT_B
      if ms=$(time_restore "$arm" "$f"); then
        echo "$r,$rep,$arm,$pos,$ms" >> "$WORK/restores.csv"
      else
        inconc "restore $arm rep $rep at RTT $r never opened its socket"
      fi
      pos=$((pos + 1))
    done
  done
  note "recorded ${RESTORES} pairs at RTT ${r} ms"
}

main() {
  rig_init || { say "rig_init failed"; return 1; }
  rig_gate || true
  binary_is_fresh || { verdict "latency"; return 1; }
  command -v toxiproxy-server >/dev/null && command -v toxiproxy-cli >/dev/null \
    || { inconc "toxiproxy-server/toxiproxy-cli not installed (brew install toxiproxy)"; verdict "latency"; return 2; }
  proxy_up || { inconc "the proxy did not come up"; verdict "latency"; return 2; }
  proxy_gate || { verdict "latency"; return 2; }

  : > "$WORK/pushes.csv"; : > "$WORK/restores.csv"
  local r
  for r in $PUSH_RTTS; do push_leg "$r" || break; done
  if [ -s "$WORK/pushes.csv" ] && [ -n "$SIBLINGS" ]; then
    head_ "P1 verdict — predicted saving $((SIBLINGS - $(ceil_div "$SIBLINGS" "$FANOUT_A"))) round trips per push (${SIBLINGS} siblings, fanout ${FANOUT_A})"
    python3 "$HERE/analyze.py" --what push --csv "$WORK/pushes.csv" \
      --saved "$((SIBLINGS - $(ceil_div "$SIBLINGS" "$FANOUT_A")))" --null-ms 60 > "$WORK/push-verdict.txt"
    tally "$WORK/push-verdict.txt"
  fi

  if seed_restore_repo; then
    for r in $RESTORE_RTTS; do restore_leg "$r"; done
    local saved=$((SEED_FILES - $(ceil_div "$SEED_FILES" "$FANOUT_A")))
    [ -s "$WORK/restores.csv" ] || { inconc "no restore was timed"; verdict "latency"; return 2; }
    head_ "P2 verdict — predicted saving ${saved} round trips per restore (${SEED_FILES} files, fanout ${FANOUT_A})"
    python3 "$HERE/analyze.py" --what restore --csv "$WORK/restores.csv" \
      --saved "$saved" --null-ms 300 > "$WORK/restore-verdict.txt"
    tally "$WORK/restore-verdict.txt"
    # Faster is worth nothing if it is wrong: the last restore of each
    # arm must be the repository that was pushed.
    local arm
    for arm in A B; do
      if git -C "$WORK/restore-$arm.git" fsck --strict --no-progress >/dev/null 2>&1 \
         && [ "$(git -C "$WORK/restore-$arm.git" rev-parse refs/heads/main 2>/dev/null)" = "$SEED_TIP" ]; then
        ok "arm $arm's last restore passes fsck --strict and is at the seeded tip"
      else
        bad "arm $arm's last restore is not the repository that was pushed"
      fi
    done
  fi

  say ""; say "CSVs: $WORK/pushes.csv $WORK/restores.csv (KEEP=1 to retain)"
  verdict "latency"
}

trap 'proxy_down; rig_clean' EXIT
main "$@"
