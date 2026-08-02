#!/usr/bin/env bash
# Keep one privileged sleeper pod per node so `nodesh.sh` can exec instead of
# spawning.
#
#   ./scripts/nodesh-daemon.sh up      # start them (idempotent)
#   ./scripts/nodesh-daemon.sh down    # remove them
#   ./scripts/nodesh-daemon.sh status
#
# WHY. nodesh spawns a pod per call: ~15-20s of schedule/pull/run/reap. A
# single throughput diag snapshots every data server before AND after, so a
# 5-DS fleet pays 10-12 pod lifecycles — 3-4 minutes of churn around a
# 45-second measurement. The measurement was never the slow part.
#
# These pods do NOTHING until exec'd into: `sleep`, no CPU, no I/O, no
# mounts. They do hold a privileged hostPID/hostNetwork slot, so take them
# down when the campaign ends — `down` is not optional hygiene, a forgotten
# privileged sleeper is a real footgun on a shared cluster.
set -uo pipefail
: "${KUBECONFIG:?set KUBECONFIG}"
IMAGE=${NODESH_IMAGE:-busybox:1.36}
ACTION=${1:-up}

nodes() { kubectl get nodes -o jsonpath='{range .items[*]}{.metadata.name}{"\n"}{end}'; }
name_for() { echo "nodesh-daemon-$(echo "$1" | tr -cd 'a-z0-9')"; }

case "$ACTION" in
  up)
    for n in $(nodes); do
      p=$(name_for "$n")
      if kubectl get pod "$p" -o jsonpath='{.status.phase}' 2>/dev/null | grep -q Running; then
        echo "  = $p (already up)"
        continue
      fi
      kubectl delete pod "$p" --ignore-not-found --wait=false >/dev/null 2>&1
      cat <<YAML | kubectl apply -f - >/dev/null
apiVersion: v1
kind: Pod
metadata: {name: $p, labels: {app: nodesh-daemon}}
spec:
  nodeName: $n
  hostPID: true
  hostNetwork: true
  hostIPC: true
  dnsPolicy: ClusterFirstWithHostNet
  tolerations: [{operator: Exists}]
  restartPolicy: Always
  containers:
  - name: sh
    image: $IMAGE
    command: ["sleep", "100000000"]
    securityContext: {privileged: true}
YAML
      echo "  + $p"
    done
    kubectl wait --for=condition=Ready pod -l app=nodesh-daemon --timeout=180s >/dev/null 2>&1 \
      && echo "✓ all sleepers ready — nodesh now execs (~1s) instead of spawning (~20s)" \
      || echo "! some sleepers not Ready; nodesh falls back to spawning for those"
    ;;
  down)
    kubectl delete pod -l app=nodesh-daemon --ignore-not-found --wait=false >/dev/null 2>&1
    echo "✓ sleepers removed"
    ;;
  status)
    kubectl get pods -l app=nodesh-daemon -o wide --no-headers 2>/dev/null \
      | awk '{printf "  %-34s %-10s %s\n", $1, $3, $7}' || echo "  none"
    ;;
  *) echo "usage: nodesh-daemon.sh up|down|status"; exit 2;;
esac
