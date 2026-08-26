#!/usr/bin/env bash
# Bring up the flint-lean cluster drill rig: provision, strip what lean
# does not need, sideload the image, stand up the store.
#
#   ./bringup.sh <cluster-name>
#
# Requires the trove backend to be RUNNING (it needs sudo to open the
# WireGuard utun, so a human starts it):
#
#   ! sudo launchctl load /Library/LaunchDaemons/com.trove.serve.plist
#   -- or --
#   ! cd ~/github/trove && fish scripts/aws-live-serve.fish
#
# SHAPE (all-spot, control plane included, per the standing rule):
#   1 x i4i.xlarge  CP      ~$0.115/hr  — 4 vCPU because a 1000-pod burst
#                                         is an API-SERVER burst first
#  12 x i4i.large   workers ~$0.041/hr  — 1000 pods at kubelet's 110/node
#                                         needs 10; 12 leaves headroom and
#                                         one node to sacrifice in leg A2
#   ≈ $0.61/hr, single region, single AZ (cross-AZ transfer is the cost
#   trap, not the instances).
set -u
cd "$(dirname "$0")"

NAME=${1:?usage: bringup.sh <cluster-name>}
TROVE=${TROVE:-$HOME/github/trove}
BASE=https://localhost:8080/api/v1
TOKEN=trove-dummy-token
REGION=${AWS_REGION:-us-west-1}
CP_TYPE=i4i.xlarge
WORKER_TYPE=i4i.large
WORKERS=${WORKERS:-12}

api() { # method path [body]
  local m=$1 p=$2 b=${3:-}
  if [ -n "$b" ]; then
    curl -fsS -k --noproxy '*' --max-time 60 -X "$m" "$BASE$p" \
      -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" -d "$b"
  else
    curl -fsS -k --noproxy '*' --max-time 60 -X "$m" "$BASE$p" \
      -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json"
  fi
}

echo "── preflight"
api GET /projects > /dev/null 2>&1 || {
  echo "FAIL: the trove backend is not answering on $BASE."
  echo "      Start it (it needs sudo) and re-run:"
  echo "        ! sudo launchctl load /Library/LaunchDaemons/com.trove.serve.plist"
  exit 1
}
[ -f lean-drill.tar ] || { echo "FAIL: lean-drill.tar missing — run the build first"; exit 1; }
# The arch trap: kind images are aarch64, i4i is Intel. Wrong arch = a
# whole provision wasted on ImagePullBackOff-equivalents.
docker image inspect flint-lean-drill:cluster --format '{{.Architecture}}' 2>/dev/null | grep -c amd64 > /dev/null \
  || { echo "FAIL: flint-lean-drill:cluster is not amd64"; exit 1; }
echo "  ok: backend up, image tar present and amd64"

echo "── provisioning all-spot project '$NAME' ($WORKERS x $WORKER_TYPE + $CP_TYPE spot CP)"
PID=$(api POST /projects "$(printf '{"name":"%s","cloudCredentialId":2,"workerCount":%s,"aws":{"region":"%s","controlPlaneInstanceType":"%s","controlPlaneNodeType":"aws_spot","workerNodeType":"aws_spot","workerInstanceType":"%s"}}' \
  "$NAME" "$WORKERS" "$REGION" "$CP_TYPE" "$WORKER_TYPE")" | jq -r .id)
[ -n "$PID" ] && [ "$PID" != "null" ] || { echo "FAIL: project create"; exit 1; }
echo "  project id = $PID"

api POST /servers/create "$(printf '{"projectId":%s,"name":"%s-control-plane","role":"Kubemaster","nodeType":"aws_spot","awsRegion":"%s","awsInstanceType":"%s"}' \
  "$PID" "$NAME" "$REGION" "$CP_TYPE")" > /dev/null
for i in $(seq 1 "$WORKERS"); do
  api POST /servers/create "$(printf '{"projectId":%s,"name":"%s-worker%s","role":"Kubeworker","nodeType":"aws_spot","awsRegion":"%s","awsInstanceType":"%s"}' \
    "$PID" "$NAME" "$i" "$REGION" "$WORKER_TYPE")" > /dev/null
  # Two nodes created in the same second collide on one timestamp-derived
  # name, and deleting the duplicate EVICTS THE LIVE NODE.
  sleep 2
done
api POST /project-deployment/commit "$(printf '{"projectId":%s}' "$PID")" > /dev/null
echo "  committed — polling"

for i in $(seq 1 120); do
  ST=$(fish "$TROVE/scripts/aws-live-drive.fish" status "$NAME" 2>/dev/null | tail -20)
  printf '%s' "$ST" | grep -c '"status": *"Ready"' > /dev/null && break
  sleep 15
done

KUBECONFIG_PATH=/tmp/kubeconfig-$NAME.yaml
curl -sk --noproxy '*' -X POST "$BASE/kubeconfig/download" \
  -H "Authorization: Bearer $TOKEN" -H "Content-Type: application/json" \
  -d "{\"projectId\":$PID}" -o "$KUBECONFIG_PATH"
export KUBECONFIG=$KUBECONFIG_PATH
kubectl get nodes 2>&1 | head -20
kubectl wait --for=condition=Ready nodes --all --timeout=600s > /dev/null || {
  echo "FAIL: not all nodes Ready"; exit 1; }
echo "  ok: cluster Ready — export KUBECONFIG=$KUBECONFIG_PATH"

# lean needs NO flint storage: no SPDK, no CSI, no NFS. Removing it frees
# CPU and, more importantly, pod slots on 110-maxPods nodes where the
# burst needs every one of them.
echo "── removing the flint CSI stack (lean uses none of it)"
helm uninstall flint-csi -n flint-system > /dev/null 2>&1 || true

echo "── dedicating a store node"
STORE_NODE=$(kubectl get nodes -l '!node-role.kubernetes.io/control-plane' \
  -o jsonpath='{.items[0].metadata.name}')
kubectl label node "$STORE_NODE" lean-drill/store=yes --overwrite > /dev/null
kubectl taint node "$STORE_NODE" lean-drill/store=yes:NoSchedule --overwrite > /dev/null
echo "  ok: $STORE_NODE reserved for MinIO"

echo "── sideloading flint-lean-drill:cluster onto every node"
kubectl apply -f sideload.yaml > /dev/null
kubectl -n lean-drill rollout status deploy/imgsrv --timeout=300s > /dev/null
IMGSRV=$(kubectl -n lean-drill get pods -l app=imgsrv -o jsonpath='{.items[0].metadata.name}')
kubectl -n lean-drill cp lean-drill.tar "$IMGSRV:/srv/lean-drill.tar"
echo "  ok: image tar served in-cluster"

WANT=$(kubectl get nodes --no-headers | grep -c .)
for i in $(seq 1 60); do
  OKN=$(kubectl -n lean-drill logs -l app=sideload --tail=5 2>/dev/null | grep -c SIDELOAD-OK)
  [ "$OKN" -ge "$WANT" ] && break
  FAILN=$(kubectl -n lean-drill logs -l app=sideload --tail=5 2>/dev/null | grep -c SIDELOAD-FAIL)
  [ "$FAILN" -gt 0 ] && { kubectl -n lean-drill logs -l app=sideload --tail=3; echo "FAIL: sideload"; exit 1; }
  sleep 10
done
[ "${OKN:-0}" -ge "$WANT" ] || { echo "FAIL: only $OKN/$WANT nodes imported the image"; exit 1; }
echo "  ok: image on all $WANT nodes"

echo "── store"
kubectl apply -f minio.yaml > /dev/null
kubectl -n lean-drill rollout status deploy/minio --timeout=300s > /dev/null || { echo "FAIL: minio"; exit 1; }
kubectl -n lean-drill wait --for=condition=complete job/make-bucket --timeout=300s > /dev/null || { echo "FAIL: bucket"; exit 1; }
kubectl -n lean-drill wait --for=condition=Ready pod/mc --timeout=300s > /dev/null || { echo "FAIL: mc"; exit 1; }
echo "  ok: MinIO + bucket + oracle up"

echo
echo "RIG READY.  export KUBECONFIG=$KUBECONFIG_PATH && ./run-cluster-drill.sh"
echo "TEARDOWN:   fish $TROVE/scripts/aws-live-drive.fish teardown $NAME  (then verify zero instances)"
