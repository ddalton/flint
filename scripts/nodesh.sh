#!/usr/bin/env bash
# Run a shell command in a node's HOST namespace, from the Mac.
#
#   ./scripts/nodesh.sh <node> '<shell command>'
#   echo '<script>' | ./scripts/nodesh.sh <node> -
#
# The trove clusters have no SSH from the Mac — the kube API is the only
# path in — so anything that needs the real host (/proc/net/dev per NIC,
# /proc/1/mountstats for the kubelet's NFS mounts, /sys CPU hotplug, perf)
# has to go through a privileged pod that nsenters PID 1.
#
# The command is base64'd into the pod spec rather than interpolated, so
# quotes, newlines and $ in the script never have to survive a trip through
# JSON and two shells. That is not fastidiousness: an earlier campaign shipped
# `\&\&` into a container entrypoint and printed `sh: 1: [: missing ]` on
# every start.
#
# SPEED. A fresh pod per invocation costs 15-20s (schedule, pull, run, reap).
# That is fine for one-off pokes and ruinous for measurement: a diag run that
# snapshots 5 data servers before and after wraps ~3-4 MINUTES of pod churn
# around a 45-second measurement, and the churn is what you end up waiting
# on. `nodesh-daemon.sh up` leaves one sleeper pod per node behind; this
# script then execs into it (~1s) and only falls back to spawning when no
# sleeper exists. Same command, same output, one order of magnitude.
set -uo pipefail
NODE=${1:?usage: nodesh.sh <node> '<command>' | <node> -}
shift
if [ "${1:-}" = "-" ]; then CMD=$(cat); else CMD="$*"; fi
[ -n "$CMD" ] || { echo "nodesh: empty command" >&2; exit 2; }

# Fast path: a long-lived sleeper already on this node.
SLEEPER="nodesh-daemon-$(echo "$NODE" | tr -cd 'a-z0-9')"
if kubectl get pod "$SLEEPER" -o jsonpath='{.status.phase}' 2>/dev/null | grep -q Running; then
  exec kubectl exec "$SLEEPER" -- nsenter -t 1 -m -u -i -n -p -- \
    sh -c "echo $(printf '%s' "$CMD" | base64 | tr -d '\n') | base64 -d | sh"
fi

IMAGE=${NODESH_IMAGE:-busybox:1.36}
POD="nodesh-$(echo "$NODE$CMD$$" | cksum | cut -d' ' -f1)"
B64=$(printf '%s' "$CMD" | base64 | tr -d '\n')

# stdinOnce+attach is what makes `kubectl run` block until the command is
# done and stream its output; --rm reaps the pod either way.
kubectl run "$POD" --rm --attach --restart=Never --image="$IMAGE" \
  --overrides="{
    \"apiVersion\":\"v1\",
    \"spec\":{
      \"nodeName\":\"$NODE\",
      \"hostPID\":true,\"hostNetwork\":true,\"hostIPC\":true,
      \"dnsPolicy\":\"ClusterFirstWithHostNet\",
      \"tolerations\":[{\"operator\":\"Exists\"}],
      \"containers\":[{
        \"name\":\"nodesh\",\"image\":\"$IMAGE\",
        \"command\":[\"nsenter\",\"-t\",\"1\",\"-m\",\"-u\",\"-i\",\"-n\",\"-p\",\"--\",
                     \"sh\",\"-c\",\"echo $B64 | base64 -d | sh\"],
        \"securityContext\":{\"privileged\":true},
        \"stdin\":false,\"tty\":false
      }]
    }
  }" 2>/dev/null
