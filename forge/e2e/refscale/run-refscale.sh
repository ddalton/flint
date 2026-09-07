#!/usr/bin/env bash
# REF SCALE — what a push costs as the repository's branch count grows.
#
# The rate leg on runcc (2026-09-06, `forge/e2e/results/tiers-rate-ab-2026-09-06.log`)
# measured 32 pushers against forge three times in a row and watched
# BOTH arms fall from 17.4 to 9.7 pushes a second over three minutes.
# The arms differed in the fold planner and fell together, so the fold
# was not it. What grew between the repetitions was the branch count:
# every pusher pushed to a NEW branch, about 900 a minute.
#
# The suspect is X19, written down in the simplification note the day
# before and never measured: **the snapshot carries a map of every ref
# and is rewritten by every batch**. If that is the mechanism then a
# push's fixed cost is not fixed at all — it is linear in the number of
# refs the repository holds, and it is paid by every push regardless of
# what that push touches.
#
# This rig measures the curve directly, on one binary against a real S3
# API: at each rung of a branch ladder, the median latency of a lone
# push that moves ONE ref, and the sizes of the two objects a batch
# rewrites whole.
#
# THE CONTROL IS GIT ITSELF. `receive-pack` advertises every ref to the
# client before a push begins, so ANY git server gets slower as the ref
# count grows and some of the curve is not forge's at all. Every rung
# therefore also times the identical push to a PLAIN bare repository
# holding the identical refs, with no hook, no syncer and no bucket.
# What forge owns is the difference.
#
#   bash forge/e2e/refscale/run-refscale.sh
#   ARM=batch bash forge/e2e/refscale/run-refscale.sh   # derived files per batch
#   ARM=timer bash forge/e2e/refscale/run-refscale.sh   # ...and on a timer
#   RUNGS="0 2000 8000" PROBES=15 bash forge/e2e/refscale/run-refscale.sh
#
# WHAT WOULD FALSIFY THE SUSPICION. A forge curve that tracks the plain
# one, or a curve that grows while both O(refs) objects stay small. The
# rig prints every column rather than a verdict, so "it got slower"
# cannot be read as "the ref map did it" without the sizes agreeing and
# without the control staying flat.
set -uo pipefail
HERE=$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)
MINIO_NAME=${MINIO_NAME:-flint-refscale-minio}
MINIO_PORT=${MINIO_PORT:-9107}
BUCKET=${BUCKET:-refscale}
export WORK=${WORK:-/tmp/fc-refscale}
rm -rf "$WORK"; mkdir -p "$WORK"
# shellcheck source=../composition/rig.sh
. "$HERE/../composition/rig.sh"

ARM=${ARM:-timer}           # timer = derived files on a 60 s timer; batch = per batch
case "$ARM" in timer) DERIVED=60;; batch) DERIVED=0;;
  *) echo "ARM must be timer or batch"; exit 2;; esac
RUNGS=${RUNGS:-"0 2000 8000"}
PROBES=${PROBES:-15}        # lone pushes timed at each rung
CHUNK=${CHUNK:-1000}        # branches created per push
PFX=${PFX:-tenant/refscale-$ARM}

trap rig_clean EXIT
rig_init || { echo "rig_init failed"; exit 1; }
binary_is_fresh || exit 1
rig_purge "$PFX/"

now_ms() { python3 -c 'import time;print(int(time.time()*1000))'; }
obj_size() { aws s3api head-object --bucket "$BUCKET" --key "$1" \
               --query ContentLength --output text 2>/dev/null || echo 0; }

new_bare_repo "$WORK/a.git"
# Folds and repacks off: this rig is about refs, and a pack event
# inside a probe window would be measured as a ref cost.
forge_up a "$WORK/a.git" "$PFX" \
  "FLINT_FORGE_BATCH_WINDOW_MS=0" \
  "FLINT_FORGE_DERIVED_EVERY_SECS=$DERIVED" \
  "FLINT_FORGE_FOLD_FACTOR=0" \
  "FLINT_FORGE_REPACK_THRESHOLD=1000000" \
  "FLINT_FORGE_BATCH_MAX=1"
wait_key "$PFX/git/epoch" 30 >/dev/null || { inconc "the syncer never claimed"; exit 2; }

# The control: a bare repository with no hooks at all, carrying the
# same refs, pushed to over the same transport.
git init --bare -q "$WORK/plain.git"
new_clone "$WORK/a.git" "$WORK/wc"
WC=$WORK/wc
printf 'seed\n' > "$WC/f"; git_c "$WC" add f; git_c "$WC" commit -qm seed
FORGE_SOCKET=/tmp/fc-a.sock push "$WC" HEAD:refs/heads/main > "$WORK/seed.log" 2>&1
grep -q "remote rejected\|error:" "$WORK/seed.log" && { inconc "the seed push did not land"; exit 2; }
TIP=$(git_c "$WC" rev-parse HEAD)

# The probe branch exists from the start, so a probe MOVES a ref rather
# than creating one: a create and a move cost the same here, and using
# one branch keeps the ladder's rungs comparable.
FORGE_SOCKET=/tmp/fc-a.sock push "$WC" "HEAD:refs/heads/probe" > /dev/null 2>&1
git_c "$WC" remote add plain "$WORK/plain.git"
git_c "$WC" push -q plain "HEAD:refs/heads/main" "HEAD:refs/heads/probe" 2>/dev/null

made=0
printf '\narm %s (derived files every %s s)\n' "$ARM" "$DERIVED"
printf '%-7s %-10s %-10s %-11s %-12s %-12s %-10s\n' \
  refs forge_ms plain_ms forge-plain snapshot_B inforefs_B usinfo_ms
for rung in $RUNGS; do
  # ── climb to this rung ────────────────────────────────────────────
  while [ "$made" -lt "$rung" ]; do
    n=$(( rung - made )); [ "$n" -gt "$CHUNK" ] && n=$CHUNK
    specs=""
    for i in $(seq 1 "$n"); do specs="$specs $TIP:refs/heads/fleet/b$((made+i))"; done
    # shellcheck disable=SC2086
    FORGE_SOCKET=/tmp/fc-a.sock \
      REMOTE_USER=driller FLINT_FORGE_SOCKET=/tmp/fc-a.sock \
      git -C "$WC" push origin $specs > "$WORK/mk-$rung.log" 2>&1
    grep -q "remote rejected" "$WORK/mk-$rung.log" && { inconc "creating branches failed at $made"; exit 2; }
    # shellcheck disable=SC2086
    git -C "$WC" push -q plain $specs > /dev/null 2>&1
    made=$((made + n))
  done

  # ── the probe: a lone push that moves one ref, to each remote ─────
  # Interleaved, one probe to forge then the same commit to the plain
  # control, so a machine that gets busy mid-rung slows both arms and
  # not one.
  times=""; ptimes=""
  for p in $(seq 1 "$PROBES"); do
    printf 'r%s p%s\n' "$rung" "$p" >> "$WC/f"
    git_c "$WC" add f; git_c "$WC" commit -qm "probe $rung $p"
    t=$(now_ms)
    FORGE_SOCKET=/tmp/fc-a.sock push "$WC" "+HEAD:refs/heads/probe" > "$WORK/probe.log" 2>&1
    times="$times $(( $(now_ms) - t ))"
    grep -q "remote rejected\|error:" "$WORK/probe.log" && { inconc "a probe push failed at $rung refs"; exit 2; }
    t=$(now_ms)
    git_c "$WC" push -q plain "+HEAD:refs/heads/probe" > /dev/null 2>&1
    ptimes="$ptimes $(( $(now_ms) - t ))"
  done
  MED=$(python3 -c 'import sys;v=sorted(int(x) for x in sys.argv[1].split());print(v[len(v)//2])' "$times")
  PMED=$(python3 -c 'import sys;v=sorted(int(x) for x in sys.argv[1].split());print(v[len(v)//2])' "$ptimes")

  SNAP=$(obj_size "$PFX/git/snapshot")
  IREFS=$(obj_size "$PFX/git/info/refs")
  # `update-server-info` is the git subprocess a batch runs before it
  # writes info/refs, and it reads every ref. Timed here directly, in
  # the same repository, so its share is a measurement and not a guess.
  t=$(now_ms); git -C "$WORK/a.git" update-server-info; USI=$(( $(now_ms) - t ))
  printf '%-7s %-10s %-10s %-11s %-12s %-12s %-10s\n' \
    "$rung" "$MED" "$PMED" "$((MED - PMED))" "$SNAP" "$IREFS" "$USI"
done

printf '\nplain_ms is the same push to a bare repository with the same refs and no forge:\n'
printf 'it is what git charges for advertising every ref, and no server escapes it.\n'
printf 'forge-plain is what forge adds. The two objects a batch rewrites whole are the\n'
printf 'snapshot (the ref map, X19) and info/refs (the dumb protocol); both are O(refs).\n'
