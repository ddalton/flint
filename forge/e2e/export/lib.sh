# Shared by f9-export.sh and g-agent-gvisor.sh. Sourced, not run.
#
# NO EGRESS: every read of the bucket is `kubectl exec s3cli -- aws ...`
# inside the cluster, never `aws` on the operator's machine.
NS=${NS:-agents}
DOOR=${DOOR:-http://flint-forge-door.forge-system.svc}
: "${BUCKET:?set BUCKET}"; PREFIX=${PREFIX:-exdrill}
: "${TAG:?set TAG (the drill image tag)}"
AGENT_IMAGE=${AGENT_IMAGE:-dilipdalton/flint-forge-agent-drill:$TAG}
pass=${pass:-0}; fail=${fail:-0}; pend=${pend:-0}
ok(){ pass=$((pass+1)); echo "  PASS     $1"; }
bad(){ fail=$((fail+1)); echo "  FAIL     $1"; }
pending(){ pend=$((pend+1)); echo "  PENDING  $1"; }
summary(){ echo ""; echo "══ $pass passed, $fail failed, $pend pending ══"; [ "$fail" = 0 ] || return 1; [ "$pend" = 0 ] || return 2; }

s3(){ kubectl exec -n "$NS" s3cli -- aws "$@"; }
s3cat(){ s3 s3 cp "s3://$BUCKET/$1" - 2>/dev/null; }
s3keys(){ s3 s3api list-objects-v2 --bucket "$BUCKET" --prefix "$1" --query 'Contents[].Key' --output text 2>/dev/null | tr '\t' '\n' | grep -v '^None$' | grep -v '^$'; }
snap_ref(){ s3cat "$PREFIX/$1/git/snapshot" | jq -r ".refs[\"$2\"] // \"<absent>\""; }
# git, as the repository server sees it.
gitq(){ kubectl exec -n "$NS" "deploy/forge-$1" -c syncer -- git --git-dir="/repo/$NS/$1.git" "${@:2}" 2>/dev/null; }

# agent_pod <name> <runc|gvisor> <forge|default>
#   forge:   the projected token, audience forge.chert.us (the design's agent)
#   default: the pod's DEFAULT ServiceAccount token (API-server audience),
#            handed to the same helper — the door must refuse it.
agent_pod(){
    local name=$1 rt=$2 tok=$3 rtline="" tokfile=/var/run/secrets/forge.chert.us/token
    [ "$rt" = gvisor ] && rtline="  runtimeClassName: gvisor"
    [ "$tok" = default ] && tokfile=/var/run/secrets/kubernetes.io/serviceaccount/token
    kubectl -n "$NS" delete pod "$name" --ignore-not-found --wait=true >/dev/null 2>&1
    cat <<YAML | kubectl apply -f - >/dev/null
apiVersion: v1
kind: Pod
metadata: { name: $name, namespace: $NS, labels: { role: forge-agent } }
spec:
$rtline
  serviceAccountName: agent-runner
  automountServiceAccountToken: $([ "$tok" = default ] && echo true || echo false)
  restartPolicy: Never
  containers:
    - name: agent
      image: $AGENT_IMAGE
      command: ["sleep", "infinity"]
      env:
        - { name: FLINT_FORGE_TOKEN_FILE, value: $tokfile }
        - { name: POD_NAME, valueFrom: { fieldRef: { fieldPath: metadata.name } } }
      volumeMounts:
        - { name: work, mountPath: /work }
        - { name: forge-token, mountPath: /var/run/secrets/forge.chert.us, readOnly: true }
  volumes:
    - { name: work, emptyDir: {} }
    - name: forge-token
      projected:
        sources:
          - serviceAccountToken: { path: token, audience: forge.chert.us, expirationSeconds: 3600 }
YAML
    kubectl -n "$NS" wait --for=condition=Ready "pod/$name" --timeout=180s >/dev/null
}
# The agent guide's two config lines, then a shell in the pod.
asg(){ local p=$1; shift; kubectl exec -n "$NS" "$p" -- sh -c "
  git config --global credential.$DOOR.helper /usr/local/bin/flint-forge-credential
  git config --global credential.$DOOR.username pod
  git config --global user.email agent@drill; git config --global user.name \$POD_NAME
  $*"; }
# Wait until <cmd> prints <want>, up to <secs>.
wait_for(){ local want=$1 secs=$2; shift 2; local i=0
    while [ $i -lt "$secs" ]; do [ "$("$@")" = "$want" ] && return 0; sleep 5; i=$((i+5)); done; return 1; }
