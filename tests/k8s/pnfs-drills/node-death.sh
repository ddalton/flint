#!/usr/bin/env bash
#
# node-death.sh — Phase 4 drill 2: node death + out-of-service taint.
#
# The ugly one. Kubelet dies on the target DS's node (containers keep
# running — this is a CONTROL-plane death, the data plane on that node
# keeps serving until we force the move). The StatefulSet will NOT
# reschedule on its own: the pod sits Terminating forever once
# evicted, because only kubelet can confirm its death. The operator
# action this drill scripts and the runbook documents:
#
#   1. node goes NotReady (~40 s after kubelet stops)
#   2. kubectl taint nodes <node> node.kubernetes.io/out-of-service=nodeshutdown:NoExecute
#      → force-deletes the pods AND force-detaches their volumes
#        (NodeOutOfServiceVolumeDetach, GA) without the 6-minute
#        attach-detach timeouts
#   3. StatefulSet replaces the pod elsewhere; the PVC follows; the
#      per-pod ClusterIP is unchanged; NodeStage self-heals the target
#      if its export died
#   4. restore: remove the taint, restart kubelet (SSM — the node
#      cannot run pods), node returns Ready
#
# Asserts: replacement DS on another node, same ClusterIP, same
# identity stamp, writer zero errors + checksums clean. Reports the
# DS outage window and max client stall.
#
#   KUBECONFIG=... CLIENT_NODE=<node> [TARGET_DS=flint-pnfs-ds-0] \
#   [AWS_PROFILE=rolesanywhere] tests/k8s/pnfs-drills/node-death.sh
set -uo pipefail
cd "$(dirname "$0")"
. ./lib.sh

TARGET_DS=${TARGET_DS:-flint-pnfs-ds-0}
WRITER=pnfs-d2-writer
TAINTED=""
DEAD_NODE=""
INSTANCE_ID=""

restore_node() {
  set +e
  if [ -n "$TAINTED" ]; then
    kubectl taint nodes "$TAINTED" node.kubernetes.io/out-of-service- >/dev/null 2>&1
  fi
  if [ -n "$DEAD_NODE" ] && [ -n "$INSTANCE_ID" ] && command -v aws >/dev/null; then
    aws ssm send-command --region "${AWS_REGION:-us-west-1}" --instance-ids "$INSTANCE_ID" \
      --document-name AWS-RunShellScript \
      --parameters commands="systemctl start kubelet" >/dev/null 2>&1 \
      && printf '  · kubelet restart sent via SSM to %s\n' "$INSTANCE_ID"
  elif [ -n "$DEAD_NODE" ]; then
    printf '  ! MANUAL RESTORE NEEDED: systemctl start kubelet on %s\n' "$DEAD_NODE"
  fi
  cleanup_writer "$WRITER"
}
trap restore_node EXIT

need_env
step "preflight"
fleet_healthy
DS_NODE=$(kubectl get pod -n "$NS" "$TARGET_DS" -o jsonpath='{.spec.nodeName}')
DS_IP=$(kubectl get svc -n "$NS" "$TARGET_DS" -o jsonpath='{.spec.clusterIP}')
MDS_NODE=$(kubectl get pods -n "$NS" -l app=flint-pnfs-mds -o jsonpath='{.items[0].spec.nodeName}')
[ "$DS_NODE" != "$CLIENT_NODE" ] || fail "TARGET_DS is on CLIENT_NODE"
[ "$DS_NODE" != "$MDS_NODE" ] || fail "TARGET_DS shares a node with the MDS — pick another target"
# kubeadm-provisioned nodes (e.g. trove's) often have no providerID —
# fall back to matching the node's InternalIP against EC2.
INSTANCE_ID=$(kubectl get node "$DS_NODE" -o jsonpath='{.spec.providerID}' 2>/dev/null | sed 's|.*/||')
if [ -z "$INSTANCE_ID" ] && command -v aws >/dev/null; then
  NODE_IP=$(kubectl get node "$DS_NODE" -o jsonpath='{.status.addresses[?(@.type=="InternalIP")].address}')
  INSTANCE_ID=$(aws ec2 describe-instances --region "${AWS_REGION:-us-west-1}" \
    --filters "Name=private-ip-address,Values=${NODE_IP}" "Name=instance-state-name,Values=running" \
    --query "Reservations[].Instances[].InstanceId" --output text 2>/dev/null)
fi
STAMP0=$(kubectl exec -n "$NS" "$TARGET_DS" -- sh -c 'grep created_at /data/.flint-ds-identity' 2>/dev/null)
ok "target ${TARGET_DS} on ${DS_NODE} (${INSTANCE_ID}), ClusterIP ${DS_IP}"

step "writer + load"
make_writer "$WRITER"
start_load "$WRITER"
sleep 8

step "killing kubelet on ${DS_NODE}"
kubectl run kubelet-kill --image=busybox:1.36 --restart=Never \
  --overrides="{\"spec\":{\"nodeName\":\"${DS_NODE}\",\"hostPID\":true,\"containers\":[{\"name\":\"k\",\"image\":\"busybox:1.36\",\"command\":[\"nsenter\",\"-t\",\"1\",\"-m\",\"-u\",\"-i\",\"-n\",\"--\",\"sh\",\"-c\",\"systemctl stop kubelet && echo STOPPED\"],\"securityContext\":{\"privileged\":true}}]}}" >/dev/null
sleep 8
kubectl delete pod kubelet-kill --wait=false >/dev/null 2>&1
DEAD_NODE="$DS_NODE"
T0=$(date +%s)

step "waiting for NotReady (~40s node-monitor grace)"
for _ in $(seq 1 30); do
  st=$(kubectl get node "$DS_NODE" -o jsonpath='{.status.conditions[?(@.type=="Ready")].status}')
  [ "$st" != "True" ] && break
  sleep 5
done
[ "$st" != "True" ] || fail "node never went NotReady"
ok "node NotReady $(( $(date +%s) - T0 ))s after kubelet stop"

step "out-of-service taint (the operator action)"
OLD_UID=$(kubectl get pod -n "$NS" "$TARGET_DS" -o jsonpath='{.metadata.uid}')
kubectl taint nodes "$DS_NODE" node.kubernetes.io/out-of-service=nodeshutdown:NoExecute >/dev/null \
  || fail "taint failed"
TAINTED="$DS_NODE"
wait_pod_replaced "$NS" "$TARGET_DS" "$OLD_UID" 600 \
  || fail "${TARGET_DS} replacement never became Ready"
T1=$(date +%s)
NEW_NODE=$(kubectl get pod -n "$NS" "$TARGET_DS" -o jsonpath='{.spec.nodeName}')
NEW_IP=$(kubectl get svc -n "$NS" "$TARGET_DS" -o jsonpath='{.spec.clusterIP}')
[ "$NEW_NODE" != "$DS_NODE" ] || fail "replacement landed on the dead node?!"
[ "$NEW_IP" = "$DS_IP" ] || fail "ClusterIP changed: ${DS_IP} → ${NEW_IP}"
ok "replacement on ${NEW_NODE} in $(( T1 - T0 ))s from kubelet death, ClusterIP stable"

step "identity"
STAMP1=$(kubectl exec -n "$NS" "$TARGET_DS" -- sh -c 'grep created_at /data/.flint-ds-identity' 2>/dev/null)
[ "$STAMP1" = "$STAMP0" ] || fail "identity stamp changed: '${STAMP0}' → '${STAMP1}'"
ok "same volume followed the pod"

step "writer verdict"
wait_load "$WRITER" 600
[ "$LOAD_STATUS" = "OK" ] || fail "writer status: ${LOAD_STATUS}"
STALL=$(max_stall "$WRITER")
verify_load "$WRITER"
ok "writer OK; max client stall ${STALL}s"

step "restoring node (taint removal + kubelet via SSM)"
restore_node
trap - EXIT
for _ in $(seq 1 36); do
  st=$(kubectl get node "$DS_NODE" -o jsonpath='{.status.conditions[?(@.type=="Ready")].status}' 2>/dev/null)
  [ "$st" = "True" ] && break
  sleep 5
done
[ "$st" = "True" ] && ok "node ${DS_NODE} Ready again" || note "node not yet Ready — verify kubelet manually"

printf '\n✅ PASS: node death → out-of-service taint → reschedule (outage %ss, stall %ss)\n' "$(( T1 - T0 ))" "$STALL"
