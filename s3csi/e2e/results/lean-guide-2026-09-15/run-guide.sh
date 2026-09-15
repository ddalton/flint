#!/usr/bin/env bash
# The lean guide's steps, verbatim, against the PUBLISHED 1.54.0 charts on kind.
# MinIO stands in for "my-bucket" (the only substitution: endpoint + keys).
set -uo pipefail
export KUBECONFIG=/private/tmp/claude-503/-Users-ddalton-github-flint/9cb47d44-3183-4b37-ade3-cabacb1cc5b5/scratchpad/guide/kc
export HELM_CACHE_HOME=/private/tmp/claude-503/-Users-ddalton-github-flint/9cb47d44-3183-4b37-ade3-cabacb1cc5b5/scratchpad/guide/helmcache
say() { echo; echo "\$ $*"; }
must() { say "$@"; bash -c "$*" 2>&1; rc=$?; echo "[exit $rc]"; [ $rc -eq 0 ] || { echo "STOP: a required step failed"; exit 1; }; }
run() { say "$@"; bash -c "$*" 2>&1; echo "[exit $?]"; }
kind delete cluster --name lean-guide >/dev/null 2>&1; kind create cluster --name lean-guide --kubeconfig "$KUBECONFIG" 2>&1 | tail -1
kubectl version 2>/dev/null | grep Server
# --- stand-in bucket ---
kubectl create namespace flint-system >/dev/null
kubectl -n flint-system apply -f - >/dev/null <<'EOF'
apiVersion: apps/v1
kind: Deployment
metadata: { name: minio }
spec:
  selector: { matchLabels: { app: minio } }
  template:
    metadata: { labels: { app: minio } }
    spec:
      containers:
        - name: minio
          image: quay.io/minio/minio:latest
          args: ["server", "/data"]
          env: [{ name: MINIO_ROOT_USER, value: guidekey }, { name: MINIO_ROOT_PASSWORD, value: guidesecret }]
---
apiVersion: v1
kind: Service
metadata: { name: minio }
spec: { selector: { app: minio }, ports: [{ port: 9000 }] }
EOF
kubectl -n flint-system rollout status deploy/minio --timeout=300s >/dev/null
kubectl -n flint-system run mc --image=quay.io/minio/mc:latest --restart=Never --command -- sh -c 'until mc alias set m http://minio:9000 guidekey guidesecret; do sleep 2; done; mc mb m/my-bucket; sleep 86400' >/dev/null
kubectl -n flint-system wait --for=condition=ready pod/mc --timeout=300s >/dev/null
echo "stand-in bucket ready"

# --- 1. install ---
run 'kubectl -n flint-system create secret generic s3 --from-literal=AWS_ACCESS_KEY_ID=guidekey --from-literal=AWS_SECRET_ACCESS_KEY=guidesecret --from-literal=AWS_REGION=us-west-1'
must 'helm install flint-lean oci://registry-1.docker.io/dilipdalton/flint-lean --version 0.11.0 -n flint-system --set operatorCredentialsSecret=s3 --set endpoint=http://minio.flint-system.svc:9000'
must 'helm install flint-s3-csi oci://registry-1.docker.io/dilipdalton/flint-s3-csi --version 0.3.0 -n flint-system --set broker.static.secretRef=s3 --set node.region=us-west-1'
must 'kubectl -n flint-system rollout status deploy/flint-lean --timeout=300s'
must 'kubectl -n flint-system rollout status ds/flint-s3-csi-node --timeout=300s'
must 'kubectl -n flint-system rollout status deploy/flint-s3-broker --timeout=300s'
run 'kubectl get csidriver s3.csi.chert.us'
run 'kubectl -n flint-system get pods'
run 'kubectl -n flint-system get pods -o jsonpath="{range .items[*]}{.spec.containers[*].image}{\"\\n\"}{end}" | sort -u'
run 'kubectl -n flint-system logs deploy/flint-lean | tail -3'

# --- 2. workspace ---
run 'kubectl create namespace agents'
run 'kubectl -n agents create serviceaccount agent'
run 'kubectl -n agents create serviceaccount viewer'
cat > /tmp/guide-workspace.yaml <<'EOF'
apiVersion: chert.us/v1alpha1
kind: FlintLeanWorkspace
metadata:
  name: proj1
  namespace: agents
spec:
  projectId: team-a/proj1
  bucket: my-bucket
  keyPrefix: tenants/proj1
  region: us-west-1
  endpoint: http://minio.flint-system.svc:9000
  floorSecs: 60
  uid: 1000
  gid: 1000
  consumers:
    serviceAccounts: [agent]
    readOnlyServiceAccounts: [viewer]
EOF
must 'cat /tmp/guide-workspace.yaml; kubectl apply -f /tmp/guide-workspace.yaml'
sleep 20
run 'kubectl -n agents get flintleanworkspace proj1 -o jsonpath="{range .status.conditions[*]}{.type}={.status} {.reason}{\"\\n\"}{end}"'

# --- 3. pod ---
cat > /tmp/guide-pod.yaml <<'EOF'
apiVersion: v1
kind: Pod
metadata:
  name: agent-1
  namespace: agents
spec:
  serviceAccountName: agent
  securityContext: { runAsUser: 1000, runAsGroup: 1000, runAsNonRoot: true }
  volumes:
    - name: ws
      csi:
        driver: s3.csi.chert.us
        volumeAttributes: { chert.us/workspace: proj1 }
  containers:
    - name: agent
      image: busybox:1.36
      command: ["sh", "-c", "sleep 86400"]
      workingDir: /workspace
      volumeMounts: [{ name: ws, mountPath: /workspace }]
EOF
must 'kubectl apply -f /tmp/guide-pod.yaml'
must 'kubectl -n agents wait --for=condition=ready pod/agent-1 --timeout=600s'
run 'kubectl -n agents get pod agent-1'
run 'kubectl -n flint-workers get pods'
run 'kubectl -n agents exec agent-1 -- ls -la /workspace /workspace/.flint'

# --- 4. publish on demand ---
run 'kubectl -n agents exec agent-1 -- sh -c "echo hello > /workspace/notes.txt && printf %s \"{\\\"nonce\\\":\\\"task-42\\\"}\" > /workspace/.flint/publish && until grep -qs task-42 /workspace/.flint/publish.ack; do sleep 1; done; cat /workspace/.flint/publish.ack"'

# --- 5. verify ---
run 'kubectl -n flint-system exec mc -- mc ls --recursive m/my-bucket/tenants/proj1/'
run 'kubectl -n flint-system exec mc -- mc stat m/my-bucket/tenants/proj1/.flint/lean/current'

# --- read-only agent ---
sed -e 's/name: agent-1/name: viewer-1/' -e 's/serviceAccountName: agent/serviceAccountName: viewer/' -e 's/driver: s3.csi.chert.us/driver: s3.csi.chert.us\n        readOnly: true/' /tmp/guide-pod.yaml | python3 -c 'import sys; print(sys.stdin.read().replace("\\n", "\n"))' > /tmp/guide-viewer.yaml
run 'cat /tmp/guide-viewer.yaml; kubectl apply -f /tmp/guide-viewer.yaml'
run 'kubectl -n agents wait --for=condition=ready pod/viewer-1 --timeout=600s'
run 'kubectl -n agents exec viewer-1 -- cat /workspace/notes.txt'
run 'kubectl -n agents exec viewer-1 -- sh -c "echo x > /workspace/x.txt"'
run 'kubectl -n agents exec viewer-1 -- cat /workspace/.flint/capabilities.json'
# a viewer pod WITHOUT readOnly: refused
sed -e 's/name: viewer-1/name: viewer-rw/' -e '/readOnly: true/d' /tmp/guide-viewer.yaml > /tmp/guide-viewer-rw.yaml
run 'kubectl apply -f /tmp/guide-viewer-rw.yaml'
sleep 45
run 'kubectl -n agents get events --field-selector involvedObject.name=viewer-rw -o jsonpath="{range .items[*]}{.reason}: {.message}{\"\\n\"}{end}" | tail -2'
run 'kubectl -n flint-system exec deploy/flint-s3-broker -- true 2>/dev/null; kubectl -n flint-system run st --rm -i --restart=Never --image=busybox:1.36 -- wget -qO- http://flint-s3-broker.flint-system.svc/v1/status 2>/dev/null | head -c 400'

# --- teardown ---
run 'kubectl -n agents delete pod agent-1 viewer-1 viewer-rw'
run 'kubectl -n agents delete flintleanworkspace proj1'
run 'helm uninstall flint-s3-csi -n flint-system'
run 'helm uninstall flint-lean -n flint-system'
run 'kubectl -n flint-system exec mc -- mc ls --recursive m/my-bucket/tenants/proj1/files/'
echo GUIDE-RUN-DONE
