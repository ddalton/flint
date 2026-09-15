#!/usr/bin/env bash
# The lean tree's layout AS DEPLOYED: the s3.csi.chert.us chart at its
# default (workers.quota=false, a plain directory) against workers.quota=true
# (the loop-mounted image), measured from inside a tenant pod on the same
# node (tree-bench-pod.py), and the host's own mount table checked for each
# arm so an arm cannot measure the other layout.
#
# Runs against a cluster set up by s3csi/e2e/run-s3csi.sh setup (STORE=s3),
# with the images pushed under TAG. Env: KUBECONFIG CTX TAG BUCKET S3_REGION
# NODE, REPS (5), SIZE_MIB (256), FILES (5000), FSYNC_SECS (15), OUT.
set -u
cd "$(dirname "$0")"
REPO=$(cd ../../.. && pwd)
K="kubectl --context $CTX"
NS=s3-tenants SYS=flint-system
REPS=${REPS:-5} SIZE_MIB=${SIZE_MIB:-256} FILES=${FILES:-5000} FSYNC_SECS=${FSYNC_SECS:-15}
OUT=${OUT:-$REPO/lean/e2e/perf/results-tree-layout-drill.out}
S3_ENDPOINT=${S3_ENDPOINT:-https://s3.$S3_REGION.amazonaws.com}
: > "$OUT"

$K -n $NS create configmap tree-bench --from-file=tree-bench-pod.py=tree-bench-pod.py --dry-run=client -o yaml | $K apply -f - >/dev/null
cat <<EOF | $K apply -f - >/dev/null
apiVersion: v1
kind: ServiceAccount
metadata: { name: editor, namespace: $NS }
---
apiVersion: chert.us/v1alpha1
kind: FlintLeanWorkspace
metadata: { name: perf-tree, namespace: $NS }
spec:
  projectId: team-a/perf-tree
  bucket: $BUCKET
  keyPrefix: perf/tree
  endpoint: $S3_ENDPOINT
  region: $S3_REGION
  floorSecs: 3600
  sizeLimitGib: 20
  uid: 1001
  gid: 1001
  consumers: { serviceAccounts: [editor] }
EOF

for arm in plain loop; do
    q=false; [ "$arm" = loop ] && q=true
    helm --kube-context "$CTX" upgrade --install flint-s3-csi "$REPO/flint-s3-csi-chart" -n $SYS \
        --set node.image.tag="$TAG" --set workers.passthroughImage.tag="$TAG" --set workers.leanImage.tag="$TAG" \
        --set broker.backend=static --set broker.static.secretRef=s3-broker-static --set broker.replicas=1 \
        --set node.region="$S3_REGION" --set node.credsLifetimeSecs=120 \
        $([ "$arm" = loop ] && echo --set workers.quota=true) >/dev/null
    $K -n $SYS rollout status ds/flint-s3-csi-node --timeout=600s >/dev/null
    env_q=$($K -n $SYS get ds flint-s3-csi-node -o jsonpath='{.spec.template.spec.containers[0].env[?(@.name=="FLINT_S3CSI_QUOTA")].value}')
    echo "ARM $arm FLINT_S3CSI_QUOTA=$env_q" | tee -a "$OUT"
    cat <<EOF | $K apply -f - >/dev/null
apiVersion: v1
kind: Pod
metadata: { name: tree-bench, namespace: $NS }
spec:
  serviceAccountName: editor
  nodeName: $NODE
  automountServiceAccountToken: false
  restartPolicy: Never
  securityContext: { runAsNonRoot: true, runAsUser: 1001, runAsGroup: 1001, seccompProfile: { type: RuntimeDefault } }
  volumes:
    - { name: ws, csi: { driver: s3.csi.chert.us, volumeAttributes: { chert.us/workspace: perf-tree } } }
    - { name: bench, configMap: { name: tree-bench } }
  containers:
    - name: agent
      image: python:3.12-alpine
      command: ["sh", "-c", "trap 'exit 0' TERM INT; sleep 86400 & wait"]
      securityContext: { allowPrivilegeEscalation: false, capabilities: { drop: [ALL] } }
      volumeMounts: [{ name: ws, mountPath: /workspace }, { name: bench, mountPath: /bench }]
EOF
    i=0; until [ "$($K -n $NS get pod tree-bench -o jsonpath='{.status.phase}')" = Running ] || [ $i -ge 600 ]; do sleep 5; i=$((i + 5)); done
    u=$($K -n $NS get pod tree-bench -o jsonpath='{.metadata.uid}')
    src=$("$REPO/scripts/nodesh.sh" "$NODE" "grep '$u/volumes/kubernetes.io~csi/ws/mount ' /proc/self/mountinfo | head -1 | awk '{for (i=1;i<=NF;i++) if (\$i==\"-\") {print \$(i+1), \$(i+2); exit}}'" 2>/dev/null | tail -1)
    echo "ARM $arm host bind source: $src" | tee -a "$OUT"
    case "$arm:$src" in
        plain:*loop*) echo "VOID: the plain arm's tree is on a loop device" | tee -a "$OUT"; continue ;;
        loop:*loop*) ;;
        loop:*) echo "VOID: the loop arm's tree is not on a loop device" | tee -a "$OUT"; continue ;;
    esac
    $K -n $NS exec tree-bench -c agent -- python3 /bench/tree-bench-pod.py /workspace "$REPS" "$SIZE_MIB" "$FILES" "$FSYNC_SECS" 2>&1 \
        | sed "s/^{/{\"layout\":\"$arm\",/" | tee -a "$OUT"
    $K -n $NS delete pod tree-bench --wait=true --timeout=300s >/dev/null 2>&1
done
echo DONE | tee -a "$OUT"
