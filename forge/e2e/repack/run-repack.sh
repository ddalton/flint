#!/usr/bin/env bash
# REPACK AMPLIFICATION — how many bytes reach S3 per byte pushed.
#
# `maybe_repack` runs `git repack -a -d -b`, which collapses the
# repository into ONE pack, and then uploads every pack the snapshot
# does not already name. That pack is the whole repository. So every
# time the pack count passes `repack_threshold` (24), a repository
# re-uploads all of itself — and the serving loop is inside that
# upload, so pushes queue behind it. At the design's 10 GB envelope
# the scale drill measured such an upload at 262 s.
#
# Nobody had ever measured how much that costs per push. This does,
# end to end, against the shipped binary:
#
#   amplification = bytes of pack uploaded / bytes of content pushed
#
# WHY TWO REPOSITORY SHAPES. `repack --geometric` is the obvious
# alternative — roll up only enough packs to keep a progression — and
# whether it helps depends on a detail that is easy to get wrong:
# git's progression is over OBJECT COUNTS, not bytes. A source tree
# has many small objects, so its big pack dwarfs each push's handful
# and is left alone. A repository whose size is a few large blobs has
# a low object count, the progression does not hold, and geometric
# rolls up everything exactly as the full repack does. Measuring one
# shape only would have answered the wrong question.
#
#   ./run-repack.sh                  # ~6 min, both shapes + the control
#   ARMS="source" ./run-repack.sh
#   KEEP=1 ./run-repack.sh
#
# TIERS (X18, docs/plans/forge-compaction-tiers-design.md). The
# `tiers-source` and `tiers-blob` arms run the SAME shapes with
# `FLINT_FORGE_FOLD_FACTOR=$FOLD` (2): geometric folds of plain packs
# beside the loop, the base rebuilt at 50 % tier growth (the hourly
# cadence lifted so the rig can see one). The shipped arms keep
# `FOLD_FACTOR=0`, the full repack at THRESHOLD, and are the control.
# The design's re-sized settings are the ones that separate the arms:
#
#   ARMS="blob tiers-blob source tiers-source" BLOB_SEED_MB=512 PUSHES=100 ./run-repack.sh
#
# (at the shipped 96 MiB / 30 pushes a full repack every 24 is cheap
# and the tiers are WORSE — the regime, not the rule). Pre-registered
# there: tiers-blob ≤ 5.5x, tiers-source ≤ 25x; blob ≥ 12x, source ≥ 40x.
set -uo pipefail

HERE=$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)
BUCKET=${BUCKET:-repack}
MINIO_NAME=${MINIO_NAME:-flint-repack-minio}
MINIO_PORT=${MINIO_PORT:-9105}
# shellcheck source=../composition/rig.sh
. "$HERE/../composition/rig.sh"

PUSHES=${PUSHES:-30}          # enough to cross the default threshold of 24
THRESHOLD=${THRESHOLD:-24}    # the shipped default
ARMS=${ARMS:-"source blob control"}
SRC_DIRS=${SRC_DIRS:-120}     # source arm: dirs x files small objects
SRC_FILES=${SRC_FILES:-100}
BLOB_SEED_MB=${BLOB_SEED_MB:-96}
BLOB_PUSH_MB=${BLOB_PUSH_MB:-2}
FOLD=${FOLD:-2}               # the tiers arms' geometric factor
MAX_TIERS_BLOB=${MAX_TIERS_BLOB:-5.5}     # pre-registered ceilings (design §8.1)
MAX_TIERS_SOURCE=${MAX_TIERS_SOURCE:-25}
has_arm() { case " $ARMS " in *" $1 "*) return 0;; esac; return 1; }
mib() { python3 -c "print(f'{int($1)/1048576:.1f}')"; }

# Every pack object under the prefix, as "<key> <size>".
pack_objects() {  # pack_objects <prefix>
  aws s3api list-objects-v2 --bucket "$BUCKET" --prefix "$1/git/objects/pack/" \
    --query 'Contents[].[Key,Size]' --output text 2>/dev/null | tr '\t' ' '
}

# Bytes of pack that have EVER appeared under the prefix.
#
# Packs are content-named and immutable, so a key that shows up is an
# upload that happened; the sweep deleting it later does not un-spend
# it. Counting the bucket's CURRENT contents would have measured the
# opposite of the thing — a repack makes the bucket SMALLER while
# spending the most.
#
# Only packs are counted. The snapshot, the epoch cell and the two
# derived files are overwritten in place at fixed keys, and together
# they are a few KiB against MiB of pack.
record_new() {  # record_new <prefix> <seen-file> -> bytes uploaded since the last call
  local prefix=$1 seen=$2 total=0 key size
  touch "$seen"
  while read -r key size; do
    [ -n "$key" ] || continue
    if ! grep -qxF "$key" "$seen"; then
      echo "$key" >> "$seen"
      total=$((total + size))
    fi
  done < <(pack_objects "$prefix")
  echo "$total"
}

# ── the two repository shapes ────────────────────────────────────────
seed_source() {  # seed_source <clone>
  python3 - "$1" "$SRC_DIRS" "$SRC_FILES" <<'PY'
import os, random, sys
root, dirs, files = sys.argv[1], int(sys.argv[2]), int(sys.argv[3])
random.seed(7)
for d in range(dirs):
    os.makedirs(f"{root}/src/d{d}", exist_ok=True)
    for f in range(files):
        with open(f"{root}/src/d{d}/f{f}.txt", "w") as fh:
            fh.write("".join(random.choice("abcdefghijklmnop \n") for _ in range(400)))
PY
}
push_source() {  # push_source <clone> <n> -> bytes of content added
  local c=$1 n=$2 i bytes=0
  for i in 1 2 3 4 5; do
    python3 -c "
import random,sys
random.seed($n*10+$i)
open('$c/src/d0/n${n}_$i.txt','w').write(''.join(random.choice('abcdefghijklmnop \n') for _ in range(400)))"
    bytes=$((bytes + 400))
  done
  echo $bytes
}
seed_blob() {  # seed_blob <clone>
  local c=$1 i=0
  mkdir -p "$c/blobs"
  while [ $i -lt "$BLOB_SEED_MB" ]; do
    dd if=/dev/urandom of="$c/blobs/b$i" bs=1048576 count=1 status=none; i=$((i+1))
  done
}
push_blob() {  # push_blob <clone> <n> -> bytes of content added
  dd if=/dev/urandom of="$1/blobs/p$2" bs=1048576 count="$BLOB_PUSH_MB" status=none
  echo $((BLOB_PUSH_MB * 1048576))
}

# ── one arm ──────────────────────────────────────────────────────────
# <name> <shape> <threshold>. The control arm is the SAME shape with
# the repack put out of reach: if it does not come back at ~1.0x, the
# rig is measuring something other than the repack and neither number
# below means anything.
run_arm() {  # run_arm <name> <shape> <threshold> [fold-factor] [max-ratio]
  local name=$1 shape=$2 threshold=$3
  local fold=${4:-0}
  local max_ratio=${5:-0}
  if [ "$fold" = 0 ]; then
    head_ "$name — ${PUSHES} pushes of a ${shape}-shaped repository, repack threshold ${threshold}"
  else
    head_ "$name — ${PUSHES} pushes of a ${shape}-shaped repository, compaction tiers at factor ${fold}"
  fi
  local prefix="rp/$name"
  local bare="$WORK/$name.git"
  local clone="$WORK/$name"
  local seen="$WORK/$name.seen"
  local log="$WORK/$name.csv"
  local sock="/tmp/fc-$name.sock"
  rig_purge "$prefix"; rm -f "$seen" "$log"
  new_bare_repo "$bare" || { bad "could not init $name"; return 1; }
  new_clone "$bare" "$clone"
  git -C "$clone" config user.name driller; git -C "$clone" config user.email driller@invalid

  "seed_$shape" "$clone"
  git_c "$clone" add -A >/dev/null 2>&1
  git_c "$clone" commit -q -m seed >/dev/null 2>&1

  # shellcheck disable=SC2086
  forge_up "$name" "$bare" "$prefix" FLINT_FORGE_ENDPOINT="$ENDPOINT" \
    FLINT_FORGE_BATCH_WINDOW_MS=0 FLINT_FORGE_REPACK_THRESHOLD="$threshold" \
    FLINT_FORGE_FOLD_FACTOR="$fold" FLINT_FORGE_BASE_REBUILD_MIN_SECS=0 \
    FLINT_FORGE_ORPHAN_GRACE_SECS=100000
  local n=0
  while [ ! -S "$sock" ] && [ $n -lt 60 ]; do sleep 1; n=$((n+1)); done
  [ -S "$sock" ] || { inconc "$name: the syncer never opened its socket"; stop_arm "$name"; return 1; }

  local rc; FORGE_SOCKET="$sock" push "$clone" "HEAD:refs/heads/main" > "$WORK/$name.push" 2>&1; rc=$?
  [ $rc -eq 0 ] || { bad "$name: the seed push failed (rc=$rc)"; sed 's/^/        /' "$WORK/$name.push" | head -5; stop_arm "$name"; return 1; }
  local up; up=$(record_new "$prefix" "$seen")
  echo "0 $up $up" >> "$log"
  note "seed: the import uploaded $(mib "$up") MiB"

  local i content
  for i in $(seq 1 "$PUSHES"); do
    content=$("push_$shape" "$clone" "$i")
    git_c "$clone" add -A >/dev/null 2>&1
    git_c "$clone" commit -q -m "push $i" >/dev/null 2>&1
    FORGE_SOCKET="$sock" push "$clone" "HEAD:refs/heads/main" > "$WORK/$name.push" 2>&1 || {
      bad "$name: push $i failed"; sed 's/^/        /' "$WORK/$name.push" | head -4; break; }
    # The repack runs AFTER the report, so its upload lands slightly
    # after the push returns. Give it the room, then account.
    sleep 1
    up=$(record_new "$prefix" "$seen")
    echo "$i $content $up" >> "$log"
  done
  if [ "$fold" != 0 ]; then
    # A fold uploads beside the loop and may land after the last
    # push's accounting: wait for the syncer to report no fold in
    # flight (its log), then account once more as a content-less row.
    local w=0 last
    while [ $w -lt 180 ]; do
      last=$(grep -E "folding|rebuilding the base|committed|fold failed|fold not committed|fold stalled" "$WORK/forge-$name.log" | tail -1)
      case "$last" in *folding*|*"rebuilding the base"*) sleep 1; w=$((w+1));; *) break;; esac
    done
    sleep 3
    up=$(record_new "$prefix" "$seen")
    echo "$((PUSHES + 1)) 0 $up" >> "$log"
    note "$name: folds committed $(grep -c 'fold committed' "$WORK/forge-$name.log"), base rebuilds $(grep -c 'base rebuild committed' "$WORK/forge-$name.log"), fold failures $(grep -c 'fold failed\|fold not committed\|fold stalled' "$WORK/forge-$name.log")"
  fi
  stop_arm "$name"
  python3 "$HERE/amplify.py" --name "$name" --csv "$log" --threshold "$threshold" --fold "$fold" --max-ratio "$max_ratio" > "$WORK/$name.verdict"
  local kind rest
  while read -r kind rest; do
    case "$kind" in PASS) ok "$rest";; FAIL) bad "$rest";; INCONCLUSIVE) inconc "$rest";; *) note "$rest";; esac
  done < "$WORK/$name.verdict"
}

stop_arm() {
  local p="$WORK/forge-$1.pid" pid
  [ -f "$p" ] || return 0
  pid=$(cat "$p"); kill "$pid" 2>/dev/null
  for _ in $(seq 1 60); do kill -0 "$pid" 2>/dev/null || break; sleep 0.1; done
  kill -9 "$pid" 2>/dev/null; rm -f "$p"; return 0
}

# ── what geometric would cost instead ────────────────────────────────
# Pure git, no syncer: take each arm's repository and compare the bytes
# `repack -a -d -b` rewrites against `repack --geometric=2 -d`. What a
# repack rewrites is what forge then has to upload, so this is the size
# of the prize without changing a line of the syncer.
geometric_probe() {  # geometric_probe <name>
  # One `local` per line: `local a=$1 b="$a"` expands `$a` BEFORE the
  # builtin assigns it, which under `set -u` is an unbound variable.
  local name=$1
  local src="$WORK/$name.git"
  [ -d "$src" ] || return 0
  head_ "G — what a geometric repack would rewrite instead, on the ${name} repository"
  local full="$WORK/$name.full"
  local geo="$WORK/$name.geo"
  rm -rf "$full" "$geo"; cp -R "$src" "$full"; cp -R "$src" "$geo"
  ls "$src"/objects/pack/*.pack 2>/dev/null | xargs -n1 basename | sort > "$WORK/$name.before"
  local packs_before; packs_before=$(wc -l < "$WORK/$name.before" | tr -d ' ')
  if [ "$packs_before" -lt 2 ]; then
    note "$name has $packs_before pack(s) — nothing for either repack to roll up"
    return 0
  fi
  local fb gb
  if ! git -C "$full" repack -a -d -b -q > "$WORK/$name.full.err" 2>&1; then
    inconc "$name: the full repack itself failed — $(head -1 "$WORK/$name.full.err")"; return 1
  fi
  fb=$(ls "$full"/objects/pack/*.pack | xargs -n1 basename | sort | comm -13 "$WORK/$name.before" - | while read -r p; do stat -f %z "$full/objects/pack/$p"; done | awk '{s+=$1} END {print s+0}')
  # `--write-midx`, and NOT because it is tidy. A plain geometric
  # repack REFUSES outright on a repository with `pack.writeBitmaps`
  # on — "Incremental repacks are incompatible with bitmap indexes" —
  # and forge has it on for the clone path (§8). The first version of
  # this probe sent that fatal to /dev/null and reported "geometric
  # rewrites 0.0 MiB", which read as a total win and was in fact the
  # command never running. Bitmaps survive an incremental repack only
  # as a MULTI-pack index, so that is what the alternative would
  # actually have to be, and it is what is measured here.
  if ! git -C "$geo" repack --geometric=2 -d --write-midx -q > "$WORK/$name.geo.err" 2>&1; then
    inconc "$name: the geometric repack failed — $(head -1 "$WORK/$name.geo.err")"; return 1
  fi
  gb=$(ls "$geo"/objects/pack/*.pack | xargs -n1 basename | sort | comm -13 "$WORK/$name.before" - | while read -r p; do stat -f %z "$geo/objects/pack/$p"; done | awk '{s+=$1} END {print s+0}')
  local remaining; remaining=$(ls "$geo"/objects/pack/*.pack 2>/dev/null | wc -l | tr -d ' ')
  # A no-op is not a saving. Geometric leaving the repository exactly
  # as it found it means the progression already held, and the next
  # push would face the same roll-up — so it is reported, never
  # counted as a win.
  if [ "$gb" -eq 0 ] && [ "$remaining" -eq "$packs_before" ]; then
    note "$name: geometric was a NO-OP (still $remaining packs) — nothing was rolled up, so nothing was saved either"
    return 0
  fi
  note "$name: from $packs_before packs — full rewrites $(mib "$fb") MiB into 1 pack, geometric rewrites $(mib "$gb") MiB into $remaining"
  if [ "$gb" -lt $((fb / 2)) ]; then
    ok "on the $name shape geometric rewrites $(mib "$gb") MiB where the full repack rewrites $(mib "$fb") MiB"
  else
    ok "on the $name shape geometric rewrites $(mib "$gb") MiB against the full repack's $(mib "$fb") MiB — no saving worth having"
  fi
}

main() {
  rig_init || { say "rig_init failed"; return 1; }
  binary_is_fresh || { verdict "repack"; return 1; }
  has_arm source  && run_arm source  source "$THRESHOLD"
  has_arm blob    && run_arm blob    blob   "$THRESHOLD"
  # The control: the same source shape with the repack out of reach.
  has_arm control && run_arm control source 100000
  # The tiers (X18): the same shapes under geometric folds.
  has_arm tiers-source && run_arm tiers-source source "$THRESHOLD" "$FOLD" "$MAX_TIERS_SOURCE"
  has_arm tiers-blob   && run_arm tiers-blob   blob   "$THRESHOLD" "$FOLD" "$MAX_TIERS_BLOB"
  has_arm source  && geometric_probe source
  has_arm blob    && geometric_probe blob
  say ""; say "per-push CSVs (push, content bytes, uploaded bytes): $WORK/*.csv (KEEP=1 to retain)"
  verdict "repack"
}

trap 'rig_clean' EXIT
main "$@"
