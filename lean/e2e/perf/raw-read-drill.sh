#!/usr/bin/env bash
# The raw HTTP read path vs the SDK read path, on real S3 with TLS.
#
# ONE binary, two arms: `sdk` is the store's SDK reads (the default),
# `raw` is FLINT_SYNC_RAW_READS=true — GET/HEAD over pooled HTTP/1.1
# with SigV4 by hand (flint-store `rawread.rs`). Everything else is
# identical: same manifest, same CRC verification, same writes.
#
# WHAT IT MEASURES
#   small   20,000 x 8 KiB: files/s and the syncer's CPU per file
#           (main + worker threads, from /proc) at fanout 128/256/512.
#           On a fresh bucket S3's per-prefix GET rate (~5,500/s per
#           partition, growing under load) caps BOTH arms, so CPU/file
#           is the client's cost either way and files/s is the wall's;
#           the `warm` mode below shows whether the wall moves.
#   big     8 x 64 MiB: the ranged arm (`get_range_segments`, If-Match
#           per range) — MiB/s and CPU per MiB.
#   identity  each arm's tree sha256-diffed against the seed, once per
#           arm per size: the raw path is a new client, and a client
#           that is fast and wrong is worse than the SDK.
#   warm    N minutes of back-to-back small checkouts on ONE arm,
#           files/s per run: S3 partitions a hot prefix over time and
#           this is what the growth looks like, if it comes.
#
# GUARDS: bytes identical across arms and fanouts; ranged=0 on small,
# ranged=8 on big; identity diff empty; the raw arm's rows must not be
# missing (a checkout that FAILED is reported, never skipped).
#
# n=3 REPS MINIMUM, arms interleaved within each rep; quote RANGES.
#
# USAGE (on the node): FLINT_SYNC=./flint-sync DRILL_BUCKET=b ./raw-read-drill.sh seed|small [reps]|big [reps]|warm [minutes] [arm]
set -euo pipefail
cd "$(dirname "$0")"
MODE="${1:-small}"; ARG="${2:-3}"; ARG3="${3:-raw}"
: "${FLINT_SYNC:=./flint-sync}"
: "${DRILL_BUCKET:?set DRILL_BUCKET}"
: "${DRILL_PREFIX:=raw-drill}"
: "${DRILL_ROOT:=/mnt/nvme/raw}"
FANOUTS="${FANOUTS:-128 256 512}"
RESULTS="results/raw-$(date -u +%Y%m%d-%H%M%S).tsv"
mkdir -p results "$DRILL_ROOT"
IFACE=$(ip route show default | awk '{print $5; exit}')

seed_workload() {
  local dir="$DRILL_ROOT/seed-small"; rm -rf "$dir"; mkdir -p "$dir"
  for d in $(seq -f 'd%02g' 0 19); do mkdir -p "$dir/$d"; for i in $(seq -f "$d/f-%04g" 0 999); do head -c 8192 /dev/urandom > "$dir/$i"; done; done
  (cd "$dir" && find . -type f -not -path './.flint*' | sort | xargs sha256sum) > "$DRILL_ROOT/seed-small.sha256"
  local out; out=$(FLINT_SYNC_ROOT="$dir" FLINT_SYNC_BUCKET="$DRILL_BUCKET" FLINT_SYNC_PREFIX="$DRILL_PREFIX/small" "$FLINT_SYNC" barrier 2>&1)
  local up; up=$(echo "$out" | sed -n 's/.* up=\([0-9]*\).*/\1/p' | tail -1)
  [ "${up:-0}" -eq 20000 ] || { echo "SEED GUARD FAIL (small): up=${up:-?}" >&2; echo "$out" >&2; exit 1; }
  echo "seeded small: up=$up"
  local big="$DRILL_ROOT/seed-big"; rm -rf "$big"; mkdir -p "$big"
  for i in $(seq 0 7); do head -c $((64*1024*1024)) /dev/urandom > "$big/big-$i.bin"; done
  (cd "$big" && find . -type f -not -path './.flint*' | sort | xargs sha256sum) > "$DRILL_ROOT/seed-big.sha256"
  out=$(FLINT_SYNC_ROOT="$big" FLINT_SYNC_BUCKET="$DRILL_BUCKET" FLINT_SYNC_PREFIX="$DRILL_PREFIX/big" FLINT_SYNC_UPLOAD_PART_PARALLELISM=8 "$FLINT_SYNC" barrier 2>&1)
  up=$(echo "$out" | sed -n 's/.* up=\([0-9]*\).*/\1/p' | tail -1)
  [ "${up:-0}" -eq 8 ] || { echo "SEED GUARD FAIL (big): up=${up:-?}" >&2; echo "$out" >&2; exit 1; }
  echo "seeded big: up=$up"
}

sample_threads() { # <pid> <outfile>
  local pid="$1" out="$2" last="" s
  while kill -0 "$pid" 2>/dev/null; do
    s=$(for t in /proc/"$pid"/task/*/stat; do awk '{print $1, $14+$15}' "$t" 2>/dev/null || true; done) || true
    [ -n "$s" ] && last="$s"; sleep 0.2
  done
  echo "$last" > "$out"
}
cpu_of() { awk -v pid="$1" '{c=$2/100; if($1==pid) m=c; else {o+=c; n++}} END{printf "%.2f %.2f %d", m, o, n+1}' "$2"; }

one_run() { # <rep> <arm> <fanout> <size> [identity] -> TSV row + line
  local rep="$1" arm="$2" fanout="$3" size="$4" ident="${5:-0}" dir="$DRILL_ROOT/run"
  rm -rf "$dir"; mkdir -p "$dir"
  sync; echo 3 > /proc/sys/vm/drop_caches 2>/dev/null || true
  local raw=false; [ "$arm" = raw ] && raw=true
  local rx0 rx1 socks=0 errf="$dir.err"; rx0=$(awk -v i="$IFACE:" '$1==i {print $2}' /proc/net/dev)
  local t0; t0=$(date +%s.%N)
  # The process's EXACT user+sys from the shell's own accounting (a
  # sampler that keeps the last /proc reading undercounts a run that
  # ends between samples — a 1.5 s raw run at 0.2 s sampling lost up
  # to 13%, and twice read 0.00 for every worker thread).
  ( TIMEFORMAT='%U %S'; time FLINT_SYNC_ROOT="$dir" FLINT_SYNC_BUCKET="$DRILL_BUCKET" FLINT_SYNC_PREFIX="$DRILL_PREFIX/$size" \
    FLINT_SYNC_FANOUT="$fanout" FLINT_SYNC_RAW_READS="$raw" "$FLINT_SYNC" checkout >/dev/null 2>"$errf" ) 2>"$dir.time" &
  local sub=$!; sleep 0.05
  local pid; pid=$(pgrep -n -x flint-sync || echo "$sub")
  sample_threads "$pid" "$dir.threads" & local sampler=$!
  while kill -0 "$sub" 2>/dev/null; do local n; n=$(ss -Htn state established '( dport = :443 )' 2>/dev/null | wc -l); [ "$n" -gt "$socks" ] && socks=$n; sleep 0.5; done
  local rc=0; wait "$sub" || rc=$?
  wait "$sampler" || true
  local cpu_exact; cpu_exact=$(awk '{printf "%.2f", $1+$2}' "$dir.time" 2>/dev/null || echo 0)
  local t1; t1=$(date +%s.%N)
  rx1=$(awk -v i="$IFACE:" '$1==i {print $2}' /proc/net/dev)
  if [ "$rc" -ne 0 ]; then
    printf '%s\t%s\t%s\t%s\tFAILED\t0\t0\t0\t%s\t0\t0\n' "$rep" "$arm" "$fanout" "$size" "$(cpu_of "$pid" "$dir.threads")" >> "$RESULTS"
    echo "rep=$rep arm=$arm fanout=$fanout size=$size CHECKOUT FAILED rc=$rc: $(tail -n 3 "$errf" | tr '\n' ' ' | cut -c1-300)"; return 0
  fi
  local phase fetch bytes ranged mat cpu wall
  phase=$(grep -F 'flint-sync: phase' "$errf" | head -1)
  fetch=$(echo "$phase" | sed -n 's/.*fetch=\([0-9.]*\)s.*/\1/p'); bytes=$(echo "$phase" | sed -n 's/.*bytes=\([0-9]*\).*/\1/p'); ranged=$(echo "$phase" | sed -n 's/.*ranged=\([0-9]*\).*/\1/p')
  mat=$(sed -n 's/.*— \([0-9]*\) materialized.*/\1/p' "$errf" | head -1)
  cpu=$(cpu_of "$pid" "$dir.threads"); wall=$(awk -v a="$t0" -v b="$t1" 'BEGIN{printf "%.2f", b-a}')
  local diffl=-1
  if [ "$ident" = 1 ]; then
    (cd "$dir" && find . -type f -not -path './.flint*' | sort | xargs sha256sum) > "$dir.sha256"
    diffl=$(diff "$DRILL_ROOT/seed-$size.sha256" "$dir.sha256" | wc -l)
  fi
  printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$rep" "$arm" "$fanout" "$size" "$fetch" "$bytes" "$ranged" "$mat" "$cpu" "$socks" "$((rx1-rx0))" "$wall" "$diffl" "$cpu_exact" >> "$RESULTS"
  local rate per; if [ "$size" = small ]; then rate="files/s=$(awk -v m="$mat" -v f="$fetch" 'BEGIN{printf "%d", m/f}')"; per="cpu/file=$(awk -v c="$cpu_exact" -v m="$mat" 'BEGIN{printf "%dus", c/m*1e6}')"; else rate="MiB/s=$(awk -v b="$bytes" -v f="$fetch" 'BEGIN{printf "%d", b/1048576/f}')"; per="cpu/MiB=$(awk -v c="$cpu_exact" -v b="$bytes" 'BEGIN{printf "%dus", c/(b/1048576)*1e6}')"; fi
  echo "rep=$rep arm=$arm fanout=$fanout size=$size $rate $per cpu_exact=${cpu_exact}s fetch=${fetch}s wall=${wall}s bytes=$bytes ranged=$ranged materialized=$mat cpu_sampled(main others threads)=$cpu socks_peak=$socks rx=$((rx1-rx0)) sha_diff_lines=$diffl"
}

guards() { # <size> <ranged_expect>
  local bad=0
  awk -F'\t' -v r="$2" '$5=="FAILED" {print "GUARD FAIL: a checkout FAILED on " $2 "/" $3; f=1} $5!="FAILED" && $7!=r {print "GUARD FAIL: ranged=" $7 " on " $2 "/" $3; f=1} $13>0 {print "GUARD FAIL: identity diff " $13 " lines on " $2 "/" $3; f=1} END{exit f}' "$RESULTS" || bad=1
  local nb; nb=$(awk -F'\t' '$5!="FAILED"{print $6}' "$RESULTS" | sort -u | wc -l)
  [ "$nb" -eq 1 ] || { echo "GUARD FAIL: $nb distinct byte totals — the arms did different work" >&2; bad=1; }
  return $bad
}
report() { # <size>
  echo; echo "=== raw-read drill ($1) — RANGE over reps ($RESULTS) ==="
  printf '%-4s %-6s %10s %10s %12s %10s %6s\n' arm fanout min max 'cpu/file(us)' 'or MiB/s' socks
  awk -F'\t' -v size="$1" '$5!="FAILED"{k=$2" "$3; if(size=="small"){v=$8/$5; u=$14/$8*1e6} else {v=$6/1048576/$5; u=$14/($6/1048576)*1e6}
    if(!(k in mn)||v<mn[k])mn[k]=v; if(v>mx[k])mx[k]=v; if(!(k in un)||u<un[k])un[k]=u; if(u>ux[k])ux[k]=u; if($10>s[k])s[k]=$10}
    END{for(k in mn){split(k,a," "); printf "%-4s %-6s %10d %10d %6d-%-6d %6d\n", a[1], a[2], mn[k], mx[k], un[k], ux[k], s[k]}}' "$RESULTS" | sort -k1,1 -k2,2n
}

case "$MODE" in
  seed) seed_workload ;;
  small)
    for rep in $(seq 1 "$ARG"); do for f in $FANOUTS; do for arm in sdk raw; do one_run "$rep" "$arm" "$f" small "$([ "$rep" = 1 ] && [ "$f" = 128 ] && echo 1 || echo 0)"; done; done; done
    guards small 0 || { echo "GUARDS FAILED — do not quote these numbers." >&2; report small; exit 1; }; report small ;;
  big)
    for rep in $(seq 1 "$ARG"); do for arm in sdk raw; do one_run "$rep" "$arm" 128 big "$([ "$rep" = 1 ] && echo 1 || echo 0)"; done; done
    guards big 8 || { echo "GUARDS FAILED — do not quote these numbers." >&2; report big; exit 1; }; report big ;;
  warm)
    end=$(( $(date +%s) + ARG*60 )); i=0
    while [ "$(date +%s)" -lt "$end" ]; do i=$((i+1)); one_run "w$i" "$ARG3" 256 small 0 | sed "s/^/t=$(( ARG*60 - (end - $(date +%s)) ))s /"; done ;;
  *) echo "usage: $0 {seed|small [reps]|big [reps]|warm [minutes] [arm]}" >&2; exit 2 ;;
esac
