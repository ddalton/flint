#!/usr/bin/env bash
# The tenant mount-Secret guard (templates/tenant-secret-guard.yaml),
# checked through the API server's own admission with server dry-runs:
# no pod is scheduled except T13's, whose mount fails harmlessly (no CR).
#
# Uses a throwaway namespace that opts in, two ServiceAccounts (a listed
# deployer and an unlisted "other") and, unless the chart's policy is
# already installed, a copy of it rendered from this tree under a test
# name. Everything it creates is deleted on exit.
#
#   CTX=<kubectl context> s3csi/e2e/tenant-secret-guard.sh
#
# First run: kind (Kubernetes 1.34), 2026-10-28, 15/15.
set -o pipefail
CTX=${CTX:?set CTX to a kubectl context}
NS=${NS:-guard-test}
IMG=${IMG:-busybox:1.36}   # set IMG to pull through a mirror
ROOT=$(cd "$(dirname "$0")/../.." && pwd)
k() { kubectl --context "$CTX" "$@"; }

POLICY=flint-s3-csi-tenant-secret-guard
if ! k get validatingadmissionpolicy "$POLICY" >/dev/null 2>&1; then
  POLICY=guardtest-tenant-secret-guard
  helm template t "$ROOT/flint-s3-csi-chart" --show-only templates/tenant-secret-guard.yaml --set broker.enabled=false \
    | sed -e "s/name: flint-s3-csi-tenant-secret-guard/name: $POLICY/" -e "s/policyName: flint-s3-csi-tenant-secret-guard/policyName: $POLICY/" \
    | k apply -f - >/dev/null || exit 1
  CLEAN_POLICY=1
fi
cleanup() {
  k delete ns "$NS" --wait=false >/dev/null 2>&1
  if [ -n "${CLEAN_POLICY:-}" ]; then
    k delete validatingadmissionpolicybinding "$POLICY" >/dev/null 2>&1
    k delete validatingadmissionpolicy "$POLICY" >/dev/null 2>&1
  fi
}
trap cleanup EXIT

k apply -f - >/dev/null <<YAML || exit 1
apiVersion: v1
kind: Namespace
metadata:
  name: $NS
  labels: { chert.us/mount-secret-guard: enforce, pod-security.kubernetes.io/enforce: restricted }
  annotations: { chert.us/mount-deployers: "system:serviceaccount:$NS:deployer" }
---
apiVersion: v1
kind: ServiceAccount
metadata: { name: deployer, namespace: $NS }
---
apiVersion: v1
kind: ServiceAccount
metadata: { name: other, namespace: $NS }
---
apiVersion: v1
kind: ServiceAccount
metadata: { name: reader, namespace: $NS }
---
apiVersion: rbac.authorization.k8s.io/v1
kind: Role
metadata: { name: guard-test, namespace: $NS }
rules:
  - apiGroups: [""]
    resources: ["pods", "pods/ephemeralcontainers", "secrets"]
    verbs: ["get", "list", "create", "update", "patch"]
---
apiVersion: rbac.authorization.k8s.io/v1
kind: RoleBinding
metadata: { name: guard-test, namespace: $NS }
roleRef: { apiGroup: rbac.authorization.k8s.io, kind: Role, name: guard-test }
subjects:
  - { kind: ServiceAccount, name: deployer, namespace: $NS }
  - { kind: ServiceAccount, name: other, namespace: $NS }
YAML
pass=0; fail=0
SC='"securityContext":{"runAsNonRoot":true,"runAsUser":1000,"seccompProfile":{"type":"RuntimeDefault"}}'
CSC='"securityContext":{"allowPrivilegeEscalation":false,"capabilities":{"drop":["ALL"]}}'
pod() { # name, extra-volumes-json, extra-container-json
  echo "{\"apiVersion\":\"v1\",\"kind\":\"Pod\",\"metadata\":{\"name\":\"$1\",\"namespace\":\"$NS\"},\"spec\":{\"serviceAccountName\":\"reader\",\"automountServiceAccountToken\":false,$SC,\"containers\":[{\"name\":\"app\",\"image\":\"$IMG\",\"command\":[\"sleep\",\"3600\"],$CSC$3}],\"volumes\":[$2]}}"
}
s3vol() { echo "{\"name\":\"ws\",\"csi\":{\"driver\":\"s3.csi.chert.us\",\"readOnly\":true,\"volumeAttributes\":{\"chert.us/mount\":\"ws-cr\"},\"nodePublishSecretRef\":{\"name\":\"$1\"}}}"; }
secret() { echo "{\"apiVersion\":\"v1\",\"kind\":\"Secret\",\"metadata\":{\"name\":\"$1\",\"namespace\":\"$NS\"},\"stringData\":{\"k\":\"v\"}}"; }
check() { # label, expect(allow|deny), as-sa, json, [expected message fragment]
  out=$(echo "$4" | kubectl --context $CTX --as=system:serviceaccount:$NS:$3 create --dry-run=server -f - 2>&1); rc=$?
  got=$([ $rc -eq 0 ] && echo allow || echo deny)
  if [ "$got" = "$2" ] && { [ -z "${5:-}" ] || grep -q -- "${5:-}" <<<"$out"; }; then pass=$((pass+1)); echo "PASS $1 ($got)"; else fail=$((fail+1)); echo "FAIL $1: want $2, got $got: $out"; fi
}
check T1-deployer-creates-mount-secret   allow deployer "$(secret s3-mount-ws)"
check T2-other-creates-mount-secret      deny  other    "$(secret s3-mount-x)" "may create or change a mount Secret"
check T3-other-creates-plain-secret      allow other    "$(secret plain-x)"
check T4-deployer-mount-pod              allow deployer "$(pod t4 "$(s3vol s3-mount-ws)")"
check T5-other-mount-pod                 deny  other    "$(pod t5 "$(s3vol s3-mount-ws)")" "may create a pod that mounts"
check T6-mount-secret-without-prefix     deny  deployer "$(pod t6 "$(s3vol workspace-s3)")" "must name a Secret whose name starts with s3-mount-"
check T7-env-secretKeyRef                deny  deployer "$(pod t7 "" ',"env":[{"name":"K","valueFrom":{"secretKeyRef":{"name":"s3-mount-ws","key":"AWS_SECRET_ACCESS_KEY"}}}]')" "environment"
check T8-envFrom-secretRef               deny  other    "$(pod t8 "" ',"envFrom":[{"secretRef":{"name":"s3-mount-ws"}}]')" "environment"
check T9-secret-volume                   deny  other    "$(pod t9 '{"name":"s","secret":{"secretName":"s3-mount-ws"}}')" "not as a secret or projected volume"
check T10-projected-secret               deny  other    "$(pod t10 '{"name":"s","projected":{"sources":[{"secret":{"name":"s3-mount-ws"}}]}}')" "not as a secret or projected volume"
check T11-plain-pod-other-secret-env     allow other    "$(pod t11 "" ',"envFrom":[{"secretRef":{"name":"plain-x"}}]')"
check T12-plain-pod                      allow other    "$(pod t12 "")"

# Debug containers need a real pod: the deployer creates one. Its mount fails (no CR), which is harmless here.
pod t13 "$(s3vol s3-mount-ws)" | kubectl --context $CTX --as=system:serviceaccount:$NS:deployer create -f - >/dev/null 2>&1
DBG="{\"spec\":{\"ephemeralContainers\":[{\"name\":\"dbg\",\"image\":\"$IMG\",\"command\":[\"sleep\",\"60\"],\"targetContainerName\":\"app\",$CSC}]}}"
dbg() { # label, expect, as-sa, [message]
  out=$(kubectl --context $CTX -n $NS --as=system:serviceaccount:$NS:$3 patch pod t13 --subresource=ephemeralcontainers --type=strategic -p "$DBG" --dry-run=server 2>&1); rc=$?
  got=$([ $rc -eq 0 ] && echo allow || echo deny)
  if [ "$got" = "$2" ] && { [ -z "${4:-}" ] || grep -q -- "${4:-}" <<<"$out"; }; then pass=$((pass+1)); echo "PASS $1 ($got)"; else fail=$((fail+1)); echo "FAIL $1: want $2, got $got: $out"; fi
}
dbg T13-other-debugs-mount-pod    deny  other    "may add a debug container"
dbg T14-deployer-debugs-mount-pod allow deployer
# Fail closed: an opted-in namespace with no deployers admits no mounting pod.
kubectl --context $CTX annotate ns $NS chert.us/mount-deployers- >/dev/null
check T15-no-deployers-fail-closed deny deployer "$(pod t15 "$(s3vol s3-mount-ws)")" "may create a pod that mounts"
kubectl --context $CTX annotate ns $NS chert.us/mount-deployers=system:serviceaccount:$NS:deployer >/dev/null
echo "pass=$pass fail=$fail"
[ "$fail" -eq 0 ]
