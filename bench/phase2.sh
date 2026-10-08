#!/usr/bin/env bash
# phase2.sh -- the head-to-head (plan §5, §8): on ONE cluster, for each driver
# in order: install, run the matrix on replica count 1 and 3, uninstall,
# wipe the disks. The first driver runs again at the end (the
# return-to-first check: if it moved more than its run-to-run range, the
# environment drifted and the comparison is void).
#
#   OUT=results/<date>-phase2 ./phase2.sh
#
# Env:
#   OUT        results root (required; per-run dirs must not exist)
#   DRIVERS    order (default: "flint mayastor longhorn rook-ceph flint")
#   PASS_B     1 = after a driver's Pass A, reinstall it pinned to 1 polling
#              core and run again (drivers whose DEFAULT_CORES > 1)
#   CEILINGS   1 = run ceilings.sh first, on the raw disks (default 1)
#   RUNTIME RAMP REPS SIZE FILE_SIZE  passed through to run-fio.sh
#
# Status lines go to $OUT/status (one per step, with UTC time); a failed
# step stops the run, leaving the cluster as it was for inspection.
set -Eeuo pipefail   # -E: the ERR trap fires inside functions and subshells too
HERE="$(cd "$(dirname "$0")" && pwd)"
. "$HERE/lib/host.sh"
: "${OUT:?set OUT}"
DRIVERS="${DRIVERS:-flint mayastor longhorn rook-ceph flint}"
PASS_B="${PASS_B:-0}" CEILINGS="${CEILINGS:-1}"
export RUNTIME="${RUNTIME:-120}" RAMP="${RAMP:-30}" REPS="${REPS:-3}" SIZE="${SIZE:-100Gi}" FILE_SIZE="${FILE_SIZE:-90G}"
mkdir -p "$OUT"
status() { echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) $*" | tee -a "$OUT/status" >&2; }
trap 'status "FAILED at line $LINENO (rc=$?)"' ERR

# (bash 3.2 on macOS: no associative arrays)
version_of() {
  case "$1" in
    flint) echo 1.58.0 ;; mayastor) echo 4.6.2 ;; longhorn) echo 1.13.0 ;; rook-ceph) echo v1.21.0 ;;
    *) echo "unknown driver $1" >&2; return 1 ;;
  esac
}
install_driver() {  # <driver> <cores or "">
  local d=$1 cores=$2
  case "$d" in
    flint) VERSION=$(version_of flint) "$HERE/drivers/flint/ec2-up.sh" ;;
    *) VERSION=$(version_of "$d") CPU_CORES=${cores:-} "$HERE/drivers/$d/install.sh" ;;
  esac
}

run_driver() {  # <driver> <label> <cores or "">
  local d=$1 label=$2 cores=$3 sc
  ( . "$HERE/drivers/$d/env.sh"
    for r in 1 3; do
      sc=$([ $r = 1 ] && echo "$SC_R1" || echo "$SC_R3")
      local out="$OUT/$label-r$r"
      [ ! -e "$out" ] || { echo "$out exists" >&2; exit 1; }
      status "run $label r$r ($sc)"
      SC=$sc DRIVER="$label-r$r" CPU_PATTERNS="$CPU_PATTERNS" REACTOR_TICKS="${REACTOR_TICKS:-}" OUT="$out" \
        "$HERE/run-fio.sh" > "$out.log" 2>&1
      status "done $label r$r"
    done )
}

samplers_up
kubectl get nodes -o wide > "$OUT/nodes-start.txt"
status "start: drivers=[$DRIVERS] pass_b=$PASS_B reps=$REPS runtime=${RUNTIME}s"

# A trove cluster arrives with trove's own Flint install, which already holds
# the disks; remove it so the ceilings and every driver start from raw disks.
if kubectl get ns flint-system >/dev/null 2>&1; then
  status "remove the preinstalled Flint"
  "$HERE/drivers/flint/uninstall.sh" > "$OUT/uninstall-preinstalled.log" 2>&1
  CONFIRM_WIPE=yes "$HERE/drivers/wipe-disks.sh" > "$OUT/wipe-preinstalled.log" 2>&1
fi

if [ "$CEILINGS" = 1 ]; then
  require_clean_disks
  status "ceilings"
  OUT="$OUT/ceilings" CONFIRM_RAW_WRITE=yes RUNTIME=30 RAMP=5 "$HERE/ceilings.sh" > "$OUT/ceilings.log" 2>&1
  CONFIRM_WIPE=yes "$HERE/drivers/wipe-disks.sh" > "$OUT/wipe-ceilings.log" 2>&1
fi

done_list=""
for d in $DRIVERS; do
  n=$(( $(printf '%s\n' $done_list | grep -cx "$d" || true) + 1 )); done_list="$done_list $d"
  label="$d-a"; [ "$n" = 1 ] || label="$d-a-again$n"
  status "install $d $(version_of "$d") (Pass A, defaults)"
  install_driver "$d" "" > "$OUT/install-$label.log" 2>&1
  run_driver "$d" "$label" ""
  status "uninstall $d"
  "$HERE/drivers/$d/uninstall.sh" > "$OUT/uninstall-$label.log" 2>&1
  CONFIRM_WIPE=yes "$HERE/drivers/wipe-disks.sh" > "$OUT/wipe-$label.log" 2>&1

  default_cores=$( . "$HERE/drivers/$d/env.sh"; echo "${DEFAULT_CORES:-}" )
  if [ "$PASS_B" = 1 ] && [ "$n" = 1 ] && [ -n "$default_cores" ] && [ "$default_cores" -gt 1 ]; then
    label="$d-b1core"
    status "install $d (Pass B, 1 polling core)"
    install_driver "$d" 1 > "$OUT/install-$label.log" 2>&1
    run_driver "$d" "$label" 1
    status "uninstall $d"
    "$HERE/drivers/$d/uninstall.sh" > "$OUT/uninstall-$label.log" 2>&1
    CONFIRM_WIPE=yes "$HERE/drivers/wipe-disks.sh" > "$OUT/wipe-$label.log" 2>&1
  fi
done

kubectl get nodes -o wide > "$OUT/nodes-end.txt"
if ! diff <(awk 'NR>1{print $1,$6}' "$OUT/nodes-start.txt") <(awk 'NR>1{print $1,$6}' "$OUT/nodes-end.txt") >/dev/null; then
  status "WARNING: the node set changed during the run (spot reclaim?) -- see nodes-start/end.txt"
fi
status "ALL-DONE"
