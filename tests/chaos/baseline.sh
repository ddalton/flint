#!/usr/bin/env bash
# Capture / diff per-node storage state. Capture once after Phase-0 smoke
# (clean cluster, harness down); diff after churn loops and at teardown.
#
#   ./baseline.sh capture
#   ./baseline.sh diff
set -uo pipefail
cd "$(dirname "$0")"
. ./lib.sh

BDIR="$CHAOS_DIR/baseline"

snap_node() { # <node> <outfile>
  {
    echo "== nvme list-subsys =="
    nvme_subsys "$1"
    echo "== globalmounts =="
    globalmounts "$1"
  } > "$2"
}

case "${1:-}" in
  capture)
    need_env
    mkdir -p "$BDIR"
    for n in $(worker_nodes); do
      snap_node "$n" "$BDIR/$n.txt"
      ok "captured $n"
    done
    kubectl get volumeattachments -o name > "$BDIR/vas.txt" 2>/dev/null
    ok "captured VAs ($(wc -l < "$BDIR/vas.txt" | tr -d ' '))"
    ;;
  diff)
    need_env
    RC=0
    for n in $(worker_nodes); do
      [ -f "$BDIR/$n.txt" ] || { note "$n: no baseline"; continue; }
      CUR=$(mktemp)
      snap_node "$n" "$CUR"
      if diff -u "$BDIR/$n.txt" "$CUR" > /tmp/bdiff.$$ 2>&1; then
        ok "$n matches baseline"
      else
        note "$n DIFFERS from baseline:"; cat /tmp/bdiff.$$; RC=1
      fi
      rm -f "$CUR" /tmp/bdiff.$$
    done
    CURVA=$(kubectl get volumeattachments -o name 2>/dev/null)
    if [ "$CURVA" = "$(cat "$BDIR/vas.txt" 2>/dev/null)" ]; then
      ok "VAs match baseline"
    else
      note "VA set differs from baseline"; RC=1
    fi
    exit $RC
    ;;
  *) echo "usage: $0 capture|diff"; exit 1 ;;
esac
