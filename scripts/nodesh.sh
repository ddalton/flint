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
set -uo pipefail
NODE=${1:?usage: nodesh.sh <node> '<command>' | <node> -}
shift
if [ "${1:-}" = "-" ]; then CMD=$(cat); else CMD="$*"; fi
[ -n "$CMD" ] || { echo "nodesh: empty command" >&2; exit 2; }

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
