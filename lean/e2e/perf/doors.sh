#!/usr/bin/env bash
# doors.sh — the four doors, re-run on the shipped syncer (runcu, 2026-09-12).
# Runs ON the node as root; everything lives under $ROOT on instance-store
# NVMe. The 2026-09-10 numbers this replaces were produced by
# ranged-checkout-drill.sh (the lean arms) + door-drill.sh (P and S) on
# runcr; this file folds both into one interleaved schedule and adds the
# arms the intervening changes call for.
#
#   doors.sh seed             seed big/small/mixed into $BUCKET/$PREFIX/<w> (lean barrier)
#   doors.sh shakedown        one rep, `big` only, every arm, guards — no numbers
#   doors.sh run [reps]       the READ measurement (default 3)
#   doors.sh write [reps]     the WRITE measurement (default 3)
#
# READ arms — cold (drop_caches first), interleaved within a rep, every
# arm reading THE SAME objects at <PREFIX>/<w>/files/...:
#   L-ship   flint-sync checkout, shipped defaults, no overrides
#   L-raw    + FLINT_SYNC_RAW_READS=true
#   L-0910   FANOUT=32 INFLIGHT=512 RANGE_MIN=8 CHUNK=16 PAR=4 — the
#            2026-09-10 "ranged" arm's settings on today's binary, so the
#            binary's gains and the defaults' gains can be told apart
#   L-slow   FANOUT=1, rep 1 only: the positive control for the fetch window
#   P-32     mount-s3 --metadata-ttl minimal, no cache, 32-wide cat — the
#            2026-09-10 passthrough door exactly
#   P-1      the same 1-wide, rep 1 and `small` only: the fan-out control
#   Pw-32    warm re-read straight after P-32, nothing dropped
#   PC-32    mount-s3 --cache <NVMe dir> (metadata ttl 60), cold: the cache
#            dir wiped — the door as a deployment that cares about re-reads
#            would configure it
#   PCw-32   warm re-read straight after PC-32: the cache's whole point
#   P-meta   `find -type f` through the no-cache mount, no bytes read
#   S-32     aws s3 cp --recursive, 32 concurrent requests
#   ctl      the drop_caches control: a local NVMe tree cold vs warm, once per rep
#
# WRITE arms — a fresh prefix per rep and arm; the seed tree on NVMe;
# drop_caches before each so every arm reads the seed cold:
#   LW       flint-sync barrier from the seed dir, shipped defaults
#   SW       aws s3 cp --recursive seed -> s3, 32 concurrent
#   PW-32    32-wide cp of the seed into a mount-s3 mount of the fresh prefix
#
# GUARDS — one failure voids the run (the report still prints, marked VOID):
#   bytes/files  every read arm reports the seeded totals for its workload
#   ranged       L-ship and L-0910 report ranged>0 on big/mixed and ==0 on small
#   ctl          local cold > 2x warm, else drop_caches is not dropping
#   par          small: P-1 > 2x P-32, else fan-out does not move this rig
#   slow         small: L-slow wall > 2x L-ship wall, else the fetch window is
#                not what is being measured (on big, ranges make fan-out moot)
#   write        every write arm lands the seeded count and bytes under
#                <prefix>/files/ in S3, checked by listing, not by exit code
#
# Every figure the report prints is a RANGE over reps, never a mean.
set -uo pipefail

MODE="${1:-run}"
REPS="${2:-3}"
: "${BUCKET:?set BUCKET}"
: "${PREFIX:=ranged-drill}"
: "${ROOT:=/mnt/nvme/drill}"
: "${BIN:=/mnt/nvme/rig/flint-sync}"
: "${AWS_REGION:=us-west-1}"
export AWS_REGION AWS_DEFAULT_REGION="$AWS_REGION" AWS_MAX_ATTEMPTS=5
WORKLOADS="${WORKLOADS:-big small mixed}"
MNT="$ROOT/mnt"
CACHE="$ROOT/ms3cache"
mkdir -p "$ROOT/results" "$MNT"
TS=$(date -u +%Y%m%d-%H%M%S)
RESULTS="$ROOT/results/doors-$MODE-$TS.tsv"

# The seeded truth. Sizes are checked on the seed tree before it is
# published and against every arm afterwards.
declare -A WANT_FILES=( [big]=6           [small]=20000     [mixed]=2001       )
declare -A WANT_BYTES=( [big]=6442450944  [small]=163840000 [mixed]=4327735296 )

log() { echo "$(date -u +%H:%M:%S) $*" >&2; }
now_ms() { echo $(( $(date +%s%N) / 1000000 )); }
drop_caches() { sync; echo 3 > /proc/sys/vm/drop_caches; }
# rep  workload  arm  wall_ms  fetch_ms  bytes  files  ranged
row() { printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$@" | tee -a "$RESULTS"; }
# The tree as the SEED had it: no dot-path. A lean checkout leaves its
# own state beside the files (`.flint-sync/`, and `.flint/capabilities.json`,
# 300 bytes — the shakedown's "7 files, seeded 6"); neither is data, and
# every door is counted by the same rule.
tree_files() { find "$1" -type f -not -path '*/.*' | wc -l; }
tree_bytes() { find "$1" -type f -not -path '*/.*' -printf '%s\n' | awk '{s+=$1} END {print s+0}'; }

# ── seed ─────────────────────────────────────────────────────────────
seed_workload() { # <w>
  local w="$1" dir="$ROOT/seed-$w"
  rm -rf "$dir"; mkdir -p "$dir"
  # One urandom read split into pieces: 20k dd spawns is minutes of
  # process creation measuring nothing. urandom, not zeros: a
  # compressible seed measures the store's compression, not the network.
  many() { dd if=/dev/urandom bs=1K count=$(( $1 * $2 )) status=none | split -b "$2"K -a 6 -d - "$3"; }
  case "$w" in
    big)   for i in 1 2 3 4 5 6; do dd if=/dev/urandom of="$dir/blob-$i.bin" bs=1M count=1024 status=none; done ;;
    small) many 20000 8 "$dir/f-" ;;
    mixed) dd if=/dev/urandom of="$dir/checkpoint.bin" bs=1M count=4096 status=none; many 2000 16 "$dir/s-" ;;
  esac
  local n b; n=$(tree_files "$dir"); b=$(tree_bytes "$dir")
  [ "$n" = "${WANT_FILES[$w]}" ] && [ "$b" = "${WANT_BYTES[$w]}" ] \
    || { log "SEED FAIL [$w]: generated $n files / $b bytes, want ${WANT_FILES[$w]} / ${WANT_BYTES[$w]}"; return 1; }
  # Published by the syncer itself, and GUARDED: a key already occupied is
  # PARKED rather than overwritten, the barrier exits 0, and the manifest
  # keeps citing the previous run's bytes — every arm would then measure
  # a tree nobody intended. `parked` and `up` are on the barrier's own
  # summary line; default to the FAILING value when unparsed.
  local out parked up
  out=$(FLINT_SYNC_ROOT="$dir" FLINT_SYNC_BUCKET="$BUCKET" FLINT_SYNC_PREFIX="$PREFIX/$w" "$BIN" barrier 2>&1)
  echo "$out" >&2
  parked=$(sed -n 's/.*parked=\([0-9]*\).*/\1/p' <<<"$out" | head -1)
  up=$(sed -n 's/.*up=\([0-9]*\).*/\1/p' <<<"$out" | head -1)
  [ "${parked:-1}" = 0 ] && [ "${up:-0}" = "$n" ] \
    || { log "SEED GUARD FAIL [$w]: up=${up:-?} parked=${parked:-?}, want up=$n parked=0 (prefix not fresh?)"; return 1; }
  # The write arms reuse this tree as a PLAIN tree: no baseline, so a
  # barrier from it uploads everything, which is the measurement.
  rm -rf "$dir/.flint-sync"
  log "seeded $w: $n files, $b bytes, up=$up parked=$parked"
}

# ── L: flint-sync checkout, always into an EMPTY tree ────────────────
# A present path is skipped by the resume rule, so a dirty tree silently
# turns a measurement of the fetch window into a measurement of nothing.
lean_run() { # <rep> <w> <arm>
  local rep="$1" w="$2" arm="$3" dir="$ROOT/run-$w"
  rm -rf "$dir"; mkdir -p "$dir"
  local -a e=(FLINT_SYNC_ROOT="$dir" FLINT_SYNC_BUCKET="$BUCKET" FLINT_SYNC_PREFIX="$PREFIX/$w")
  case "$arm" in
    L-ship) ;;
    L-raw)  e+=(FLINT_SYNC_RAW_READS=true) ;;
    L-0910) e+=(FLINT_SYNC_FANOUT=32 FLINT_SYNC_FETCH_INFLIGHT_MB=512 FLINT_SYNC_RANGE_GET_MIN_MB=8
                FLINT_SYNC_RANGE_GET_CHUNK_MB=16 FLINT_SYNC_RANGE_GET_PARALLELISM=4) ;;
    L-slow) e+=(FLINT_SYNC_FANOUT=1) ;;
    *) log "unknown lean arm $arm"; return 1 ;;
  esac
  drop_caches
  local t0 t1 out
  t0=$(now_ms)
  out=$(env "${e[@]}" "$BIN" checkout 2>&1) || { log "CHECKOUT FAILED [$w/$arm]"; echo "$out" >&2; row "$rep" "$w" "$arm" FAIL - - - -; return 1; }
  t1=$(now_ms)
  local phase; phase=$(grep -F 'flint-sync: phase' <<<"$out" | tail -1)
  [ -n "$phase" ] || { log "NO PHASE LINE [$w/$arm]"; echo "$out" >&2; row "$rep" "$w" "$arm" FAIL - - - -; return 1; }
  local fetch ranged pbytes
  fetch=$(sed -n 's/.*fetch=\([0-9.]*\)s.*/\1/p' <<<"$phase")
  ranged=$(sed -n 's/.*ranged=\([0-9]*\).*/\1/p' <<<"$phase")
  pbytes=$(sed -n 's/.*bytes=\([0-9]*\).*/\1/p' <<<"$phase")
  log "  $arm[$w]: $phase"
  # Bytes and files are what is ON DISK after the verb, counted the same
  # way for every door; the phase line's own byte count is logged above.
  row "$rep" "$w" "$arm" $(( t1 - t0 )) "$(awk -v f="${fetch:-0}" 'BEGIN { printf "%d", f * 1000 }')" \
      "$(tree_bytes "$dir")" "$(tree_files "$dir")" "${ranged:--}"
  [ "$pbytes" = "$(tree_bytes "$dir")" ] || log "  note: phase bytes=$pbytes differs from on-disk $(tree_bytes "$dir")"
}

# ── P: mount-s3, read through the mount ───────────────────────────────
mount_p() { # <w> nocache|cache
  local w="$1" kind="$2" d="$MNT/$w"
  mkdir -p "$d"
  mountpoint -q "$d" && umount "$d"
  local -a args=(--read-only --prefix "$PREFIX/$w/files/" --region "$AWS_REGION")
  case "$kind" in
    nocache) args+=(--metadata-ttl minimal) ;;
    cache)   rm -rf "$CACHE"; mkdir -p "$CACHE"; args+=(--cache "$CACHE" --max-cache-size 60000) ;;
  esac
  mount-s3 "$BUCKET" "$d" "${args[@]}" > /dev/null 2>&1 || { log "MOUNT FAILED [$w/$kind]"; return 1; }
  # The mount must be REAL and POPULATED: an aborted mount-s3 leaves the
  # mount up and the listing empty, and an empty listing times as an
  # instant win — the single most likely way this arm reports a result
  # it did not measure.
  mountpoint -q "$d" || { log "NOT A MOUNTPOINT [$w/$kind]"; return 1; }
  [ -n "$(find "$d" -maxdepth 1 -type f -print -quit 2>/dev/null)" ] || { log "MOUNT EMPTY [$w/$kind]"; return 1; }
}
umount_p() { local d="$MNT/$1"; mountpoint -q "$d" && umount "$d"; sleep 1; }

# The listing is built OUTSIDE the timed window: walking 20k FUSE
# dentries 32 times over would measure `find`, not the reads.
p_read() { # <w> <par> -> "ms bytes files"
  local w="$1" par="$2" d="$MNT/$w" list="$ROOT/list-$w"
  find "$d" -type f | sort > "$list"
  local n; n=$(wc -l < "$list")
  local t0 t1 i
  t0=$(now_ms)
  for (( i = 0; i < par; i++ )); do
    awk -v p="$par" -v k="$i" 'NR % p == k' "$list" | while IFS= read -r f; do cat -- "$f"; done > /dev/null &
  done
  wait
  t1=$(now_ms)
  # Sizes from the same listing the reads walked, counted OUT of the
  # window: `cat | wc -c` would put a second process and a pipe in the
  # measured path.
  local b; b=$(xargs -a "$list" -d '\n' stat -c %s 2>/dev/null | awk '{s+=$1} END {print s+0}')
  echo "$(( t1 - t0 )) $b $n"
}
p_meta() { # <w> -> "ms files"
  local d="$MNT/$1" t0 t1 n
  t0=$(now_ms); n=$(find "$d" -type f | wc -l); t1=$(now_ms)
  echo "$(( t1 - t0 )) $n"
}

# ── S: raw S3, the thing a person actually types ─────────────────────
s3_copy() { # <w> -> "ms bytes files"
  local w="$1" dir="$ROOT/s3arm-$w"
  rm -rf "$dir"; mkdir -p "$dir"
  local t0 t1
  t0=$(now_ms)
  aws s3 cp --recursive --quiet "s3://$BUCKET/$PREFIX/$w/files" "$dir"
  t1=$(now_ms)
  echo "$(( t1 - t0 )) $(tree_bytes "$dir") $(tree_files "$dir")"
}

# ── the cache-drop control, on the load-bearing path ─────────────────
cold_control() { # -> "cold_ms warm_ms"
  local dir="$ROOT/s3arm-big"
  [ -d "$dir" ] && [ -n "$(ls -A "$dir" 2>/dev/null)" ] || { echo "0 0"; return; }
  local t0 t1 cold warm
  drop_caches
  t0=$(now_ms); cat "$dir"/* > /dev/null; t1=$(now_ms); cold=$(( t1 - t0 ))
  t0=$(now_ms); cat "$dir"/* > /dev/null; t1=$(now_ms); warm=$(( t1 - t0 ))
  echo "$cold $warm"
}

# ── write arms ───────────────────────────────────────────────────────
s3_landed() { # <prefix> -> "files bytes" as S3 lists them (never the arm's own claim)
  # `s3 ls --summarize`, not `s3api list-objects-v2 --query`: the api
  # form auto-paginates and prints one aggregate PER 1000-KEY PAGE, so a
  # 20,000-object prefix reads as "1000" — a guard that would pass a
  # tree 5% landed. Found on the seed listing, before any arm ran.
  aws s3 ls --recursive --summarize "s3://$BUCKET/$1/" 2>/dev/null \
    | awk '/Total Objects:/ {n=$3} /Total Size:/ {b=$3} END {printf "%d %d", n+0, b+0}'
}
write_run() { # <rep> <w> <arm>
  # The prefix carries this RUN's timestamp: the shakedown writes rep 1 of
  # `big` under the same arm/rep names, and a prefix that already holds
  # the objects makes mount-s3 refuse every create (EPERM, no
  # --allow-overwrite) in 162 ms while the listing guard is satisfied by
  # the leftovers — an arm that skipped its work, timed as fast.
  local rep="$1" w="$2" arm="$3" seed="$ROOT/seed-$w" pfx="w-$arm-r$rep-$TS/$w"
  rm -rf "$seed/.flint-sync"
  drop_caches
  local t0 t1 out
  case "$arm" in
    LW)
      t0=$(now_ms)
      out=$(FLINT_SYNC_ROOT="$seed" FLINT_SYNC_BUCKET="$BUCKET" FLINT_SYNC_PREFIX="$pfx" "$BIN" barrier 2>&1) \
        || { log "BARRIER FAILED [$w]"; echo "$out" >&2; row "$rep" "$w" "$arm" FAIL - - - -; return 1; }
      t1=$(now_ms)
      rm -rf "$seed/.flint-sync"
      log "  LW[$w]: $(grep -F 'flint-sync: barrier' <<<"$out" | tail -1)"
      ;;
    SW)
      t0=$(now_ms)
      aws s3 cp --recursive --quiet "$seed" "s3://$BUCKET/$pfx/files"
      t1=$(now_ms)
      ;;
    PW-32)
      local d="$MNT/w-$w" list="$ROOT/wlist-$w"
      mkdir -p "$d"; mountpoint -q "$d" && umount "$d"
      mount-s3 "$BUCKET" "$d" --prefix "$pfx/files/" --region "$AWS_REGION" > /dev/null 2>&1 \
        || { log "WRITE MOUNT FAILED [$w]"; row "$rep" "$w" "$arm" FAIL - - - -; return 1; }
      find "$seed" -type f | sort > "$list"
      t0=$(now_ms)
      for (( i = 0; i < 32; i++ )); do
        awk -v p=32 -v k="$i" 'NR % p == k' "$list" | while IFS= read -r f; do cp -- "$f" "$d/${f##*/}"; done &
      done
      wait
      # mount-s3 completes an object's PUT when the file is closed; the
      # unmount is inside the window so nothing is left in flight.
      umount "$d"
      t1=$(now_ms)
      ;;
    *) log "unknown write arm $arm"; return 1 ;;
  esac
  local n b; read -r n b <<<"$(s3_landed "$pfx/files")"
  row "$rep" "$w" "$arm" $(( t1 - t0 )) - "${b:-0}" "${n:-0}" -
}

# ── guards + report ──────────────────────────────────────────────────
guards() {
  local fail=0 w
  for w in $WORKLOADS; do
    while IFS=$'\t' read -r rep ww arm wall fetch b n ranged; do
      [ "$wall" = FAIL ] && { echo "GUARD FAIL [$w/$arm rep $rep]: the arm failed" >&2; fail=1; continue; }
      [ "$b" = "-" ] && continue
      [ "$b" = "${WANT_BYTES[$w]}" ] || { echo "GUARD FAIL [$w/$arm rep $rep]: $b bytes, seeded ${WANT_BYTES[$w]} — an arm that moved fewer bytes has not won" >&2; fail=1; }
      [ "$n" = "${WANT_FILES[$w]}" ] || { echo "GUARD FAIL [$w/$arm rep $rep]: $n files, seeded ${WANT_FILES[$w]} — this door serves a different tree" >&2; fail=1; }
      case "$arm" in
        L-ship|L-0910)
          if [ "$w" = small ]; then
            [ "$ranged" = 0 ] || { echo "GUARD FAIL [$w/$arm]: ranged=$ranged on objects under the threshold" >&2; fail=1; }
          else
            [ "${ranged:-0}" -gt 0 ] || { echo "GUARD FAIL [$w/$arm]: ranged=$ranged — the ranged path never fired, a perfect null" >&2; fail=1; }
          fi ;;
      esac
    done < <(awk -F'\t' -v w="$w" '$2==w' "$RESULTS")
  done
  local c wm
  read -r c wm <<<"$(awk -F'\t' '$3=="ctl" {c+=$4; w+=$5; n++} END {if (n) printf "%d %d", c/n, w/n}' "$RESULTS")"
  if [ -n "${c:-}" ] && [ "$c" -le $(( ${wm:-0} * 2 )) ]; then
    echo "GUARD FAIL [ctl]: local cold ${c}ms vs warm ${wm}ms — drop_caches is not dropping; every 'cold' here is a warm read" >&2; fail=1
  fi
  local p1 p32
  read -r p1 p32 <<<"$(awk -F'\t' '$2=="small" && $3=="P-1" {a=$4} $2=="small" && $3=="P-32" && !b {b=$4} END {if (a && b) printf "%d %d", a, b}' "$RESULTS")"
  if [ -n "${p1:-}" ] && [ "$p1" -le $(( p32 * 2 )) ]; then
    echo "GUARD FAIL [par]: small P-1 ${p1}ms vs P-32 ${p32}ms — fan-out does not move this rig" >&2; fail=1
  fi
  # The fan-out control lives on `small`: on `big` every object is fetched
  # as parallel RANGES, so per-object fan-out has nothing left to add
  # (the shakedown showed L-slow within 3% of L-ship there, exactly as
  # 2026-09-10 found). 20,000 objects at fan-out 1 must be far slower.
  local sl sh
  read -r sl sh <<<"$(awk -F'\t' '$2=="small" && $3=="L-slow" {a=$4} $2=="small" && $3=="L-ship" && !b {b=$4} END {if (a && b) printf "%d %d", a, b}' "$RESULTS")"
  if [ -n "${sl:-}" ] && [ "$sl" -le $(( sh * 2 )) ]; then
    echo "GUARD FAIL [slow]: small L-slow ${sl}ms vs L-ship ${sh}ms — collapsing fan-out did not move it; the fetch window is not what is measured" >&2; fail=1
  fi
  return $fail
}

report() { # <title>
  echo
  echo "=== $1 — wall ms, RANGE over reps ($RESULTS) ==="
  printf '%-7s %-8s %10s %10s %7s %12s %8s %8s\n' workload arm min max spread fetch_ms files ranged
  local w arm
  for w in $WORKLOADS; do
    for arm in $(awk -F'\t' -v w="$w" '$2==w {print $3}' "$RESULTS" | awk '!s[$0]++'); do
      awk -F'\t' -v w="$w" -v a="$arm" '
        $2==w && $3==a && $4!="FAIL" { if (min=="" || $4<min) min=$4; if ($4>max) max=$4;
                                        if ($5!="-") { if (fmin=="" || $5<fmin) fmin=$5; if ($5>fmax) fmax=$5 }
                                        n=$7; r=$8 }
        END { if (min!="") printf "%-7s %-8s %10d %10d %6.1f%% %12s %8s %8s\n", w, a, min, max, (max-min)/min*100,
                     (fmin=="" ? "-" : fmin "-" fmax), n, r }' "$RESULTS"
    done
  done
  awk -F'\t' '$3=="ctl" {printf "ctl rep %s: local NVMe cold %d ms, warm %d ms\n", $1, $4, $5}' "$RESULTS"
}

upload_results() {
  aws s3 cp "$RESULTS" "s3://$BUCKET/_rig/results/$(basename "$RESULTS")" --quiet 2>/dev/null || true
}

# ── the schedules ────────────────────────────────────────────────────
do_read() {
  local rep w ms b n c wm
  for rep in $(seq 1 "$REPS"); do
    for w in $WORKLOADS; do
      log "rep $rep / $w"
      lean_run "$rep" "$w" L-ship
      lean_run "$rep" "$w" L-raw
      lean_run "$rep" "$w" L-0910
      [ "$rep" = 1 ] && lean_run "$rep" "$w" L-slow
      mount_p "$w" nocache || { row "$rep" "$w" P-32 FAIL - - - -; continue; }
      if [ "$rep" = 1 ] && [ "$w" = small ]; then
        drop_caches; read -r ms b n <<<"$(p_read "$w" 1)";  row "$rep" "$w" P-1 "$ms" - "$b" "$n" -
      fi
      drop_caches; read -r ms b n <<<"$(p_read "$w" 32)"; row "$rep" "$w" P-32 "$ms" - "$b" "$n" -
      read -r ms b n <<<"$(p_read "$w" 32)";              row "$rep" "$w" Pw-32 "$ms" - "$b" "$n" -
      drop_caches; read -r ms n <<<"$(p_meta "$w")";      row "$rep" "$w" P-meta "$ms" - - "$n" -
      umount_p "$w"
      mount_p "$w" cache || { row "$rep" "$w" PC-32 FAIL - - - -; continue; }
      drop_caches; read -r ms b n <<<"$(p_read "$w" 32)"; row "$rep" "$w" PC-32 "$ms" - "$b" "$n" -
      read -r ms b n <<<"$(p_read "$w" 32)";              row "$rep" "$w" PCw-32 "$ms" - "$b" "$n" -
      umount_p "$w"
      drop_caches; read -r ms b n <<<"$(s3_copy "$w")";   row "$rep" "$w" S-32 "$ms" - "$b" "$n" -
    done
    read -r c wm <<<"$(cold_control)"; row "$rep" - ctl "$c" "$wm" - - -
    upload_results
  done
}
do_write() {
  local rep w arm
  for rep in $(seq 1 "$REPS"); do
    for w in $WORKLOADS; do
      log "write rep $rep / $w"
      for arm in LW SW PW-32; do write_run "$rep" "$w" "$arm"; done
    done
    upload_results
  done
}
write_guards() {
  local fail=0 w
  for w in $WORKLOADS; do
    while IFS=$'\t' read -r rep ww arm wall fetch b n ranged; do
      [ "$wall" = FAIL ] && { echo "GUARD FAIL [$w/$arm rep $rep]: the arm failed" >&2; fail=1; continue; }
      [ "$b" = "${WANT_BYTES[$w]}" ] && [ "$n" = "${WANT_FILES[$w]}" ] \
        || { echo "GUARD FAIL [$w/$arm rep $rep]: S3 lists $n files / $b bytes, seeded ${WANT_FILES[$w]} / ${WANT_BYTES[$w]} — the arm did not land the tree" >&2; fail=1; }
    done < <(awk -F'\t' -v w="$w" '$2==w' "$RESULTS")
  done
  return $fail
}

case "$MODE" in
  seed)
    for w in $WORKLOADS; do seed_workload "$w" || exit 1; done ;;
  shakedown)
    WORKLOADS=big; REPS=1
    do_read; do_write
    if guards && write_guards; then echo "SHAKEDOWN PASS — every arm ran and the guards hold"; report shakedown
    else echo "SHAKEDOWN FAIL — fix the rig before spending the reps" >&2; report shakedown; exit 1; fi ;;
  run)
    echo "results -> $RESULTS" >&2
    do_read
    if guards; then echo "GUARDS PASSED"; report READ; else echo "GUARDS FAILED — do not quote these numbers." >&2; report "READ (VOID)"; upload_results; exit 1; fi
    upload_results ;;
  write)
    echo "results -> $RESULTS" >&2
    do_write
    if write_guards; then echo "GUARDS PASSED"; report WRITE; else echo "GUARDS FAILED — do not quote these numbers." >&2; report "WRITE (VOID)"; upload_results; exit 1; fi
    upload_results ;;
  *) echo "usage: $0 {seed|shakedown|run [reps]|write [reps]}" >&2; exit 2 ;;
esac
