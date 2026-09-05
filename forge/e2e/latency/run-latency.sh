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
LEGS=${LEGS:-"p1 p2 p3 p4"}          # which legs to run
PUSH_KBPS=${PUSH_KBPS:-20000}        # P3b/P4: the upload's bandwidth through the proxy
BIG_MB=${BIG_MB:-160}                # P3b/P4: the pack, in MiB — three 64 MiB parts
has_leg() { case " $LEGS " in *" $1 "*) return 0;; esac; return 1; }

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

# ── the token, watched from the bucket ───────────────────────────────
# The epoch object's ETag changes on every renewal, so its silence is
# the takeover window a challenger would see — the same probe the
# scale drill used on real S3. curl with SigV4 HEADs it in ~5 ms, so
# the poller resolves a 1 s heartbeat comfortably; `aws s3api` would
# not (~350 ms per call).
epoch_etag() {  # epoch_etag <prefix>
  curl -sI --aws-sigv4 "aws:amz:${AWS_REGION}:s3" \
    --user "$AWS_ACCESS_KEY_ID:$AWS_SECRET_ACCESS_KEY" \
    "$ENDPOINT/$BUCKET/$1/git/epoch" -o /dev/null -w '%header{etag}' 2>/dev/null
}
# watch_epoch <prefix> <outfile>: "<ms> <etag>" lines every 200 ms
# until <outfile>.stop exists. Runs in the background; the caller
# stops it with stop_watch.
watch_epoch() {
  local prefix=$1 out=$2
  rm -f "$out" "$out.stop"
  ( while [ ! -f "$out.stop" ]; do
      printf '%s %s\n' "$(now_ms)" "$(epoch_etag "$prefix")" >> "$out"
      sleep 0.2
    done ) &
  echo $! > "$out.pid"
}
stop_watch() {  # stop_watch <outfile>
  touch "$1.stop"; wait "$(cat "$1.pid" 2>/dev/null)" 2>/dev/null; rm -f "$1.pid"
}
# silence <outfile> [from_ms] [to_ms]: the longest gap in ms between
# etag CHANGES inside the window (default: the whole file), and the
# number of changes, as "longest changes".
silence() {
  python3 - "$@" <<'PY'
import sys
path = sys.argv[1]; lo = int(sys.argv[2]) if len(sys.argv) > 2 else 0
hi = int(sys.argv[3]) if len(sys.argv) > 3 else 1 << 62
rows = []
for line in open(path):
    parts = line.split()
    if len(parts) == 2 and parts[1]:
        t = int(parts[0])
        if lo <= t <= hi: rows.append((t, parts[1]))
if not rows: print("0 0"); sys.exit()
last_change, longest, changes, prev = rows[0][0], 0, 0, rows[0][1]
for t, e in rows[1:]:
    if e != prev:
        changes += 1; longest = max(longest, t - last_change); last_change = t; prev = e
longest = max(longest, rows[-1][0] - last_change)
print(longest, changes)
PY
}

# A big, incompressible repository for the multipart legs.
build_big_clone() {  # build_big_clone <bare> <clone> <mib> -> tip on stdout
  local bare=$1 clone=$2 mb=$3 i=0
  new_bare_repo "$bare" || return 1
  new_clone "$bare" "$clone"
  mkdir -p "$clone/blobs"
  while [ $i -lt "$mb" ]; do
    dd if=/dev/urandom of="$clone/blobs/b$i" bs=1048576 count=1 status=none; i=$((i+1))
  done
  git_c "$clone" add -A >/dev/null 2>&1
  git_c "$clone" commit -q -m "a repository of $mb MiB" >/dev/null 2>&1
  git_c "$clone" rev-parse HEAD
}
# Pending multipart uploads whose key starts with <prefix>, counted
# CLIENT-SIDE from an unprefixed listing. MinIO lists an upload only
# under no prefix or its exact key — a directory prefix returns nothing
# (probed 2026-09-05, RELEASE.2025-09-07) — while S3 honours prefixes.
# The store's own `list_uploads` lists bucket-wide and filters in code
# for exactly this reason (s3.rs), so the syncer's sweep is not blind
# here; this oracle just does the same.
mpu_count() {  # mpu_count <prefix>
  aws s3api list-multipart-uploads --bucket "$BUCKET" \
    --query "length(Uploads[?starts_with(Key, '$1/git/')] || \`[]\`)" --output text 2>/dev/null || echo 0
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

# ── P3: the lease beats through the work — and only through work ─────
#
# Design §5's window, measured on real S3 at 10 GiB: the token silent
# 125 s during a push and 141 s during a restore against a 60 s
# takeover threshold, because the heartbeat was a timer arm of the
# same select! that awaited both. The renewer is now its own task,
# gated on progress. Three claims, each with a way to be wrong:
#   P3a  a restore renews while chunks land (silence <= ~2 heartbeats);
#   P3b  a multipart push renews as parts land (silence <= one part);
#   P3c  a restore whose data STOPS lets the token go quiet, and picks
#        up again when the data does — the half the peer warned about:
#        a renewer that beat for a wedged pod would trade a 60 s
#        takeover of a live pod for no takeover of a dead one.
# The control is the pre-fix binary: silence == the whole operation.
token_legs() {
  local hb=1
  head_ "P3a — the token keeps rotating through a ${SEED_FILES}-file restore at RTT 200, fanout 1, heartbeat ${hb}s"
  set_rtt 200
  local bare="$WORK/p3a.git" sock=/tmp/fc-P3A.sock watch="$WORK/p3a.watch"
  rm -rf "$bare"; new_bare_repo "$bare"; rm -f "$sock"
  watch_epoch "$SEED_PREFIX" "$watch"
  local t0; t0=$(now_ms)
  # shellcheck disable=SC2086
  forge_up P3A "$bare" "$SEED_PREFIX" $COMMON FLINT_FORGE_HEARTBEAT_SECS=$hb FLINT_FORGE_FANOUT=1
  if wait_sock "$sock" 120; then
    local t1; t1=$(now_ms); stop_watch "$watch"; stop_forge P3A
    read -r longest changes <<< "$(silence "$watch" $((t0 + 1500)) "$t1")"
    note "restore took $((t1 - t0)) ms; token changed ${changes}x, longest silence ${longest} ms"
    if [ "$longest" -le $((hb * 2500)) ] && [ "$changes" -ge 3 ]; then
      ok "the token never fell silent for more than 2.5 heartbeats during the restore"
    else
      bad "the token fell silent for ${longest} ms during a $((t1 - t0)) ms restore — the window is open"
    fi
  else
    stop_watch "$watch"; stop_forge P3A; inconc "the P3a restore never opened its socket"
  fi

  head_ "P3b — the token keeps rotating through a ${BIG_MB} MiB multipart push at ${PUSH_KBPS} KB/s, heartbeat ${hb}s"
  set_rtt 0
  local prefix="lat/p3b" bbare="$WORK/p3b.git" bclone="$WORK/p3b" bsock=/tmp/fc-P3B.sock bwatch="$WORK/p3b.watch"
  rig_purge "$prefix"
  BIG_TIP=$(build_big_clone "$bbare" "$bclone" "$BIG_MB") || { bad "could not build the big repository"; return 1; }
  # shellcheck disable=SC2086
  forge_up P3B "$bbare" "$prefix" $COMMON FLINT_FORGE_HEARTBEAT_SECS=$hb
  wait_sock "$bsock" 60 || { inconc "the P3b syncer never opened its socket"; stop_forge P3B; return 1; }
  toxiproxy-cli -h "$TOXI" toxic add -t bandwidth -n slow --upstream -a rate="$PUSH_KBPS" minio >/dev/null
  watch_epoch "$prefix" "$bwatch"
  local p0 p1 rc; p0=$(now_ms)
  FORGE_SOCKET="$bsock" push "$bclone" "HEAD:refs/heads/main" > "$WORK/p3b.push" 2>&1; rc=$?
  p1=$(now_ms); stop_watch "$bwatch"
  toxiproxy-cli -h "$TOXI" toxic remove -n slow minio >/dev/null
  stop_forge P3B
  local etag; etag=$(aws s3api list-objects-v2 --bucket "$BUCKET" --prefix "$prefix/git/objects/pack/" \
      --query 'Contents[?ends_with(Key,`.pack`)]|[0].ETag' --output text 2>/dev/null)
  if [ $rc -ne 0 ]; then bad "the push failed (rc=$rc)"; sed 's/^/        /' "$WORK/p3b.push" | head -5
  else
    case "$etag" in *-[0-9]*) ok "the push landed as a multipart object ($etag) in $((p1 - p0)) ms" ;;
      *) inconc "the pack was not composed ($etag) — this leg did not exercise the part loop" ;; esac
    read -r longest changes <<< "$(silence "$bwatch" "$p0" "$p1")"
    # One 64 MiB part is the progress granularity, at the rate the
    # push ACTUALLY ran (the bandwidth toxic is approximate: a nominal
    # 20 MB/s ran at 9 MiB/s here), so the part time is taken from the
    # measured push, not from the knob.
    local part_ms=$(( (p1 - p0) * 64 / BIG_MB ))
    note "push took $((p1 - p0)) ms; token changed ${changes}x, longest silence ${longest} ms (one part took ~${part_ms} ms)"
    if [ "$longest" -le $((part_ms + hb * 1500)) ] && [ "$changes" -ge 2 ]; then
      ok "the token never fell silent for longer than one part plus a heartbeat during the push"
    else
      bad "the token fell silent for ${longest} ms during a $((p1 - p0)) ms push — the window is open"
    fi
  fi

  head_ "P3c — a restore whose data stops lets the token go quiet, then resumes"
  set_rtt 200
  local cbare="$WORK/p3c.git" csock=/tmp/fc-P3C.sock cwatch="$WORK/p3c.watch"
  rm -rf "$cbare"; new_bare_repo "$cbare"; rm -f "$csock"
  watch_epoch "$SEED_PREFIX" "$cwatch"
  local c0; c0=$(now_ms)
  # shellcheck disable=SC2086
  forge_up P3C "$cbare" "$SEED_PREFIX" $COMMON FLINT_FORGE_HEARTBEAT_SECS=$hb FLINT_FORGE_FANOUT=1
  # Five seconds in: past the claim and the restore's preamble (at RTT
  # 200 those are a dozen round trips before the first chunk lands),
  # so the pre-stall window holds heartbeats that had progress to see.
  sleep 5
  # Stall every byte from the store for 4 s — under the SDK's 5 s
  # stalled-stream grace, so no request fails: the data simply stops,
  # which is what a wedge looks like from the renewer's chair.
  local s0; s0=$(now_ms)
  toxiproxy-cli -h "$TOXI" toxic add -t timeout -n stall --downstream -a timeout=0 minio >/dev/null
  sleep 4
  toxiproxy-cli -h "$TOXI" toxic remove -n stall minio >/dev/null
  local s1; s1=$(now_ms)
  if wait_sock "$csock" 120; then
    local c1; c1=$(now_ms); stop_watch "$cwatch"; stop_forge P3C
    read -r before_l before_c <<< "$(silence "$cwatch" $((c0 + 1500)) "$s0")"
    read -r stall_l stall_c <<< "$(silence "$cwatch" $((s0 + 1200)) "$s1")"
    read -r after_l after_c <<< "$(silence "$cwatch" "$s1" "$c1")"
    note "before the stall: ${before_c} changes; during (${s1}-${s0}=$((s1 - s0)) ms): ${stall_c}; after: ${after_c}; restore $((c1 - c0)) ms"
    [ "$before_c" -ge 1 ] && ok "the token rotated while the restore was moving" \
      || bad "the token did not rotate before the stall — the renewer is not beating through the restore"
    [ "$stall_c" -eq 0 ] && ok "the token went quiet while no data moved" \
      || bad "the token rotated ${stall_c}x while nothing moved — a wedged server would keep its lease"
    [ "$after_c" -ge 1 ] && ok "the token resumed when the data did" \
      || bad "the token did not resume after the stall"
  else
    stop_watch "$cwatch"; stop_forge P3C; inconc "the P3c restore never completed after the stall"
  fi
}

# ── P4: an interrupted upload is aborted by the successor ────────────
#
# The scale drill's S4: one kill inside a 2 GiB push left 384 MiB of
# parts in an upload nothing would ever complete or abort. Observed via
# list-multipart-uploads before the kill, never a guessed sleep; and
# both samples — pending after the kill, gone once the successor
# serves — so a sweep is observed rather than inferred from a zero.
orphan_leg() {
  head_ "P4 — a push killed inside its multipart upload leaves parts; the restart aborts them"
  set_rtt 0
  local prefix="lat/p4" bare="$WORK/p4.git" clone="$WORK/p4" sock=/tmp/fc-P4.sock
  rig_purge "$prefix"
  local tip; tip=$(build_big_clone "$bare" "$clone" "$BIG_MB") || { bad "could not build the big repository"; return 1; }
  # shellcheck disable=SC2086
  forge_up P4 "$bare" "$prefix" $COMMON
  wait_sock "$sock" 60 || { inconc "the P4 syncer never opened its socket"; stop_forge P4; return 1; }
  toxiproxy-cli -h "$TOXI" toxic add -t bandwidth -n slow --upstream -a rate="$PUSH_KBPS" minio >/dev/null
  ( FORGE_SOCKET="$sock" push "$clone" "HEAD:refs/heads/main" > "$WORK/p4.push" 2>&1 ) &
  local pusher=$! n=0 pending=0
  while [ $n -lt 60 ]; do pending=$(mpu_count "$prefix"); [ "$pending" -ge 1 ] && break; sleep 0.2; n=$((n+1)); done
  if [ "$pending" -lt 1 ]; then
    toxiproxy-cli -h "$TOXI" toxic remove -n slow minio >/dev/null
    wait $pusher 2>/dev/null; stop_forge P4
    inconc "no multipart upload was ever listed during the push — the kill could not be placed inside it"; return 1
  fi
  sleep 1
  kill -9 "$(cat "$WORK/forge-P4.pid")" 2>/dev/null; rm -f "$WORK/forge-P4.pid"
  wait $pusher 2>/dev/null
  toxiproxy-cli -h "$TOXI" toxic remove -n slow minio >/dev/null
  pending=$(mpu_count "$prefix")
  [ "$pending" -ge 1 ] && ok "the kill left ${pending} upload(s) pending (its parts are billed until aborted)" \
    || { inconc "nothing was pending after the kill — it landed outside the upload"; return 1; }
  # The same pod, restarted: same repository dir, same state dir, so
  # self-recognition rather than a takeover.
  rm -f "$sock"
  # shellcheck disable=SC2086
  forge_up P4 "$bare" "$prefix" $COMMON
  wait_sock "$sock" 120 || { inconc "the restarted syncer never opened its socket"; stop_forge P4; return 1; }
  pending=$(mpu_count "$prefix")
  [ "$pending" -eq 0 ] && ok "the restart aborted every pending upload before serving" \
    || bad "${pending} upload(s) still pending after the restart — the leak is open"
  # And the repository recovers: the same push lands.
  local rc; FORGE_SOCKET="$sock" push "$clone" "HEAD:refs/heads/main" > "$WORK/p4.push2" 2>&1; rc=$?
  [ $rc -eq 0 ] && ok "the interrupted push, retried, is accepted" || { bad "the retried push failed (rc=$rc)"; sed 's/^/        /' "$WORK/p4.push2" | head -4; }
  [ "$(mpu_count "$prefix")" -eq 0 ] && ok "and leaves nothing pending" || bad "the retried push left an upload pending"
  stop_forge P4
  [ -n "$tip" ] || true
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
  if has_leg p1; then for r in $PUSH_RTTS; do push_leg "$r" || break; done; fi
  if [ -s "$WORK/pushes.csv" ] && [ -n "$SIBLINGS" ]; then
    head_ "P1 verdict — predicted saving $((SIBLINGS - $(ceil_div "$SIBLINGS" "$FANOUT_A"))) round trips per push (${SIBLINGS} siblings, fanout ${FANOUT_A})"
    python3 "$HERE/analyze.py" --what push --csv "$WORK/pushes.csv" \
      --saved "$((SIBLINGS - $(ceil_div "$SIBLINGS" "$FANOUT_A")))" --null-ms 60 > "$WORK/push-verdict.txt"
    tally "$WORK/push-verdict.txt"
  fi

  local seeded=0
  if has_leg p2 || has_leg p3; then seed_restore_repo && seeded=1; fi
  if has_leg p2 && [ $seeded -eq 1 ]; then
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

  if has_leg p3 && [ $seeded -eq 1 ]; then token_legs; fi
  if has_leg p4; then orphan_leg; fi

  say ""; say "CSVs: $WORK/pushes.csv $WORK/restores.csv (KEEP=1 to retain)"
  verdict "latency"
}

trap 'proxy_down; rig_clean' EXIT
main "$@"
