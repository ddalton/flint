#!/usr/bin/env bash
# flint-s3-csi on a registry that needs a login (issue #85), on kind.
#
# Before #85 no pod the chart runs could be given a pull secret: a worker
# named neither imagePullSecrets nor a ServiceAccount, so on a private
# registry it sat in ImagePullBackOff and its tenant in ContainerCreating.
# Here EVERY image the chart runs (plugin, broker, both CSI sidecars, the
# worker) comes from one registry:2 that refuses an anonymous pull, and
# nothing outside the chart patches a ServiceAccount.
#
#   R0  preconditions: the registry refuses an anonymous pull and serves a
#       logged-in one, through the nodes' own containerd; the worker image
#       is on no node.
#   R1  CONTROL: global.imagePullSecrets on, workers.imagePullSecrets: []
#       and no prepull. The plugin and broker must come up (their pods pull
#       under global), and the worker must FAIL its pull, refused by the
#       registry. Without this arm R2 could pass on a registry that never
#       asked for a login.
#   R2  THE FIX: the workers take global's list. A mount on a node without
#       the worker image succeeds, the bytes are the seed's, the worker
#       names regcred, and `default` in the workers' namespace carries no
#       pull secret.
#   R3  workers.serviceAccountName: the worker runs as that account. Then
#       the admission policy, asked as the plugin itself (its pod-bound
#       token, server-side dry run): the live worker's spec is admitted
#       under its own account and refused under any other.
#   R4  prepull under the node's list: with the worker image removed from
#       every node, the plugin's prepull init container pulls it back.
#
#   ./private-registry.sh up      # kind + registry + images + rig
#   ./private-registry.sh run     # R0-R4
#   ./private-registry.sh down
#
# Runs on the Linux box. Images: TAG (default pr85) of dilipdalton/
# flint-s3-csi and flint-s3-worker in the local docker daemon, built from
# this checkout. KUBECONFIG is private to this rig.
set -u
cd "$(dirname "$0")"
REPO=$(cd ../.. && pwd)
export PATH="$HOME/bin:$PATH"

CLUSTER=${CLUSTER:-flint-s3csi-reg}
export KUBECONFIG=${KUBECONFIG_REG:-$HOME/.kube/$CLUSTER.config}
CTX=kind-$CLUSTER
K="kubectl --context $CTX"
TAG=${TAG:-pr85}
REG=${REG:-flint-reg}               # registry container name, on the kind network
REGP=$REG:5000                      # what the nodes pull from
PUSHP=127.0.0.1:${REG_HOST_PORT:-5003}   # what the box pushes to (docker allows http on loopback)
STATE=$HOME/.$CLUSTER               # htpasswd + the password (snap docker reads only $HOME)
SYS=flint-system
WNS=flint-workers
NS=s3-tenants
WORKER_IMG=$REGP/dilipdalton/flint-s3-worker:$TAG
# A pull refused for want of a login, as containerd words it. NOT the bare
# word "authorization": "repository does not exist or may require
# authorization" is also what a MISSING image says, and a control that
# passed on that would prove nothing about credentials.
AUTH_REFUSED='no basic auth credentials|401 Unauthorized|authentication required'

PASS=0
FAILED=0
bad()  { echo "  BAD: $1"; FAILED=$((FAILED + 1)); }
ok()   { PASS=$((PASS + 1)); echo "  ok: $1"; }
leg()  { echo; echo "── $1 — $2"; }
nodes() { kind get nodes --name "$CLUSTER"; }
NODE=$($K get nodes -l '!node-role.kubernetes.io/control-plane' -o jsonpath='{.items[0].metadata.name}' 2>/dev/null)

chart_up() {
    helm --kube-context "$CTX" upgrade --install flint-s3-csi "$REPO/flint-s3-csi-chart" -n $SYS \
        --set global.imageRegistry=$REGP --set 'global.imagePullSecrets={regcred}' \
        --set node.image.tag="$TAG" --set workers.passthroughImage.tag="$TAG" --set workers.leanImage.tag="$TAG" \
        --set broker.backend=static --set broker.static.secretRef=s3-broker-static --set broker.replicas=1 \
        --set node.identityModes="broker static" \
        --set node.logLevel=debug "$@"
}
rolled() {
    $K -n $SYS rollout status ds/flint-s3-csi-node --timeout=240s >/dev/null 2>&1 &&
        $K -n $SYS rollout status deploy/flint-s3-broker --timeout=240s >/dev/null 2>&1
}
# The worker image on a node, as containerd lists it ("" when absent).
img_on() { docker exec "$1" crictl images -o json 2>/dev/null | jq -r --arg i "$WORKER_IMG" '.images[] | select(.repoTags | index($i)) | .id'; }
tenant() { # name — a reader of CR datasets on $NODE
    sed -e "s#__NAME__#$1#" -e "s#__CR__#datasets#" -e "s#__NODE__#$NODE#" pt-pod.yaml.tpl | $K apply -f - >/dev/null
}
# The tenant's live worker, whatever its phase (R1's never runs).
worker_of() {
    $K -n $WNS get pods -o json 2>/dev/null | python3 -c "
import json,sys
want='$NS/$1'
for p in json.load(sys.stdin)['items']:
    if p['metadata'].get('annotations',{}).get('chert.us/tenant-pod')==want and not p['metadata'].get('deletionTimestamp'):
        print(p['metadata']['name']); break"
}
wait_worker() { # tenant secs -> worker name
    local i=0 w
    while [ $i -lt "$2" ]; do w=$(worker_of "$1"); [ -n "$w" ] && { echo "$w"; return 0; }; sleep 2; i=$((i + 2)); done
    return 1
}
gone() { # tenant: delete it and wait for its worker to leave
    local w; w=$(worker_of "$1")
    $K -n $NS delete pod "$1" --ignore-not-found --wait=true --timeout=180s >/dev/null 2>&1
    [ -z "$w" ] || $K -n $WNS wait --for=delete pod/"$w" --timeout=180s >/dev/null 2>&1
}

if [ "${1:-}" = up ]; then
    set -e
    mkdir -p "$STATE" "$(dirname "$KUBECONFIG")"
    if ! kind get clusters 2>/dev/null | grep -qx "$CLUSTER"; then
        cat >"$STATE/kind.yaml" <<'EOF'
kind: Cluster
apiVersion: kind.x-k8s.io/v1alpha4
# The node image leaves containerd's registry config_path empty, so a
# hosts.toml would be ignored and the registry (plain http) unreachable.
containerdConfigPatches:
- |-
  [plugins."io.containerd.grpc.v1.cri".registry]
    config_path = "/etc/containerd/certs.d"
nodes:
- role: control-plane
  image: kindest/node:v1.34.0@sha256:7416a61b42b1662ca6ca89f02028ac133a309a2a30ba309614e8ec94d976dc5a
- role: worker
  image: kindest/node:v1.34.0@sha256:7416a61b42b1662ca6ca89f02028ac133a309a2a30ba309614e8ec94d976dc5a
EOF
        kind create cluster --name "$CLUSTER" --config "$STATE/kind.yaml" --kubeconfig "$KUBECONFIG"
    fi
    # The registry: htpasswd (bcrypt, registry:2 reads no other) and a
    # password made here, never printed.
    [ -s "$STATE/password" ] || head -c 18 /dev/urandom | base64 | tr -d '/+=' >"$STATE/password"
    PW=$(cat "$STATE/password")
    docker run --rm --entrypoint htpasswd httpd:2.4-alpine -Bbn drill "$PW" >"$STATE/htpasswd"
    docker rm -f "$REG" >/dev/null 2>&1 || true
    docker run -d --name "$REG" --network kind -p "$PUSHP:5000" -v "$STATE:/auth:ro" \
        -e REGISTRY_AUTH=htpasswd -e REGISTRY_AUTH_HTPASSWD_REALM=flint \
        -e REGISTRY_AUTH_HTPASSWD_PATH=/auth/htpasswd registry:2 >/dev/null
    # Each node's containerd reaches it over http, through certs.d (the
    # patch above). Read the EFFECTIVE config: refuse to go on without it.
    for n in $(nodes); do
        docker exec "$n" containerd config dump 2>/dev/null | grep -qE "config_path = ['\"]/etc/containerd/certs.d['\"]" \
            || { echo "REFUSING: $n's containerd has no certs.d config_path; the registry would be unreachable" >&2; exit 1; }
        docker exec "$n" mkdir -p "/etc/containerd/certs.d/$REGP"
        printf 'server = "http://%s"\n[host."http://%s"]\n  capabilities = ["pull", "resolve"]\n' "$REGP" "$REGP" |
            docker exec -i "$n" tee "/etc/containerd/certs.d/$REGP/hosts.toml" >/dev/null
    done
    # Every image the chart will run, from the chart's own render: the
    # flint images from this daemon, the sidecars from registry.k8s.io.
    # The rig's "store" image comes from Docker Hub; it is not the chart's.
    echo "$PW" | docker login "$PUSHP" -u drill --password-stdin >/dev/null
    helm template t "$REPO/flint-s3-csi-chart" -n $SYS --set global.imageRegistry=$REGP \
        --set node.image.tag="$TAG" --set workers.passthroughImage.tag="$TAG" --set workers.leanImage.tag="$TAG" \
        --set broker.static.secretRef=x | grep -oE "$REGP/[^]\"', ]+" | sort -u >"$STATE/images.txt"
    while read -r ref; do
        path=${ref#$REGP/}
        case "$path" in
            dilipdalton/flint-s3-csi:*|dilipdalton/flint-s3-worker:*) src=$path ;;
            sig-storage/*) src=registry.k8s.io/$path; docker pull -q "$src" >/dev/null ;;
            *) echo "  (not mirrored: $ref)"; continue ;;
        esac
        docker tag "$src" "$PUSHP/$path"
        docker push -q "$PUSHP/$path" >/dev/null
        echo "  pushed $ref"
    done <"$STATE/images.txt"
    docker logout "$PUSHP" >/dev/null
    # A probe image for R0: a logged-in pull through containerd must work.
    docker pull -q busybox:1.36 >/dev/null
    echo "$PW" | docker login "$PUSHP" -u drill --password-stdin >/dev/null
    docker tag busybox:1.36 "$PUSHP/probe:1" && docker push -q "$PUSHP/probe:1" >/dev/null
    docker logout "$PUSHP" >/dev/null
    # The rig: store + seed + broker Secret, the CRD, the tenants (the
    # `datasets` CR and the `trainer` ServiceAccount).
    $K apply -f rig.yaml >/dev/null
    $K apply -f "$REPO/flint-passthrough-chart/crds/flintpassthroughmounts.yaml" >/dev/null
    $K -n $SYS rollout status deploy/minio --timeout=180s
    $K -n $SYS wait --for=condition=complete job/seed-bucket --timeout=180s
    # tenants.yaml WITHOUT its Pods: each would mount, and make a worker
    # before R1 has set the chart up.
    python3 -c '
import sys, yaml
docs = [d for d in yaml.safe_load_all(open("tenants.yaml")) if d and d.get("kind") != "Pod"]
print(yaml.safe_dump_all(docs))' | $K apply -f - >/dev/null
    # regcred in the release namespace before the install, as an operator
    # would; the workers' copy after it (the chart makes that namespace).
    $K -n $SYS create secret docker-registry regcred --docker-server="$REGP" \
        --docker-username=drill --docker-password="$PW" --dry-run=client -o yaml | $K apply -f - >/dev/null
    echo "up: cluster $CLUSTER, registry $REGP, node $NODE"
    exit 0
fi

if [ "${1:-}" = down ]; then
    kind delete cluster --name "$CLUSTER" --kubeconfig "$KUBECONFIG" || true
    docker rm -f "$REG" >/dev/null 2>&1 || true
    rm -rf "$STATE" "$KUBECONFIG"
    exit 0
fi

[ "${1:-}" = run ] || { echo "usage: $0 up|run|down" >&2; exit 2; }
[ -n "$NODE" ] || { echo "no worker node in $CTX (run up first)" >&2; exit 2; }
PW=$(cat "$STATE/password")

leg R0 "the registry needs a login, and the worker image is on no node"
out=$(docker exec "$NODE" crictl pull "$REGP/probe:1" 2>&1)
if [ $? -ne 0 ] && echo "$out" | grep -qiE "$AUTH_REFUSED"; then
    ok "an anonymous pull through the node's containerd is refused: $(echo "$out" | grep -oiE "$AUTH_REFUSED" | head -1 | cut -c1-120)"
else
    bad "an anonymous pull was NOT refused for want of a login — every arm below would prove nothing: $(echo "$out" | tail -1 | cut -c1-200)"
fi
if docker exec "$NODE" crictl pull --creds "drill:$PW" "$REGP/probe:1" >/dev/null 2>&1; then
    ok "a logged-in pull through the same containerd works (the registry is reachable over http)"
    docker exec "$NODE" crictl rmi "$REGP/probe:1" >/dev/null 2>&1
else
    bad "a logged-in pull failed: the registry is unreachable and R1 would fail for that reason"
fi
for n in $(nodes); do
    [ -z "$(img_on "$n")" ] && ok "$n has no $WORKER_IMG" || bad "$n already has the worker image"
done

leg R1 "CONTROL: workers named no pull secret, no prepull"
cat >"$STATE/r1.yaml" <<'EOF'
workers: { imagePullSecrets: [], prepull: { passthrough: false } }
EOF
chart_up -f "$STATE/r1.yaml" >/dev/null || bad "helm install failed"
if rolled; then
    ok "the plugin (and its sidecars) and the broker pulled from the private registry under global.imagePullSecrets"
else
    bad "the plugin or broker did not roll: $($K -n $SYS get pods --no-headers 2>&1 | tr '\n' ';' | cut -c1-300)"
fi
$K -n $WNS create secret docker-registry regcred --docker-server="$REGP" \
    --docker-username=drill --docker-password="$PW" --dry-run=client -o yaml | $K apply -f - >/dev/null
[ -z "$($K -n $WNS get sa default -o jsonpath='{.imagePullSecrets}')" ] \
    && ok "default in $WNS carries no pull secret (nothing outside the chart patched it)" \
    || bad "default in $WNS carries a pull secret: $($K -n $WNS get sa default -o jsonpath='{.imagePullSecrets}')"
tenant reg-a
if w=$(wait_worker reg-a 60); then
    st=""
    for _ in $(seq 1 45); do
        st=$($K -n $WNS get pod "$w" -o jsonpath='{.status.containerStatuses[0].state.waiting.reason}' 2>/dev/null)
        case "$st" in ErrImagePull|ImagePullBackOff) break ;; esac
        sleep 2
    done
    case "$st" in
        ErrImagePull|ImagePullBackOff)
            msg=$($K -n $WNS get events --field-selector involvedObject.name="$w",reason=Failed -o jsonpath='{.items[*].message}' 2>/dev/null)
            if echo "$msg" | grep -qiE "$AUTH_REFUSED"; then
                ok "the worker's pull is refused by the registry ($st): $(echo "$msg" | grep -oiE "$AUTH_REFUSED" | head -1 | cut -c1-120)"
            else
                bad "the worker failed its pull, but not for want of a login: $(echo "$msg" | cut -c1-200)"
            fi ;;
        *) bad "the worker is not failing its pull (state '${st:-none}', phase $($K -n $WNS get pod "$w" -o jsonpath='{.status.phase}')): the control does not hold" ;;
    esac
    [ -z "$($K -n $WNS get pod "$w" -o jsonpath='{.spec.imagePullSecrets}')" ] \
        && ok "with workers.imagePullSecrets: [] the worker names none" || bad "the control's worker names a pull secret"
else
    bad "no worker appeared for reg-a"
fi
[ "$($K -n $NS get pod reg-a -o jsonpath='{.status.phase}')" != Running ] \
    && ok "the tenant is not Running (it cannot mount)" || bad "the tenant is Running without a worker"
gone reg-a

leg R2 "THE FIX: the workers take global.imagePullSecrets"
chart_up --set workers.prepull.passthrough=false >/dev/null || bad "helm upgrade failed"
rolled || bad "the plugin did not roll"
[ "$($K -n $SYS get ds flint-s3-csi-node -o jsonpath='{.spec.template.spec.containers[0].env[?(@.name=="FLINT_S3CSI_WORKER_IMAGE_PULL_SECRETS")].value}')" = regcred ] \
    && ok "the plugin is told regcred" || bad "the plugin's FLINT_S3CSI_WORKER_IMAGE_PULL_SECRETS is not regcred"
[ -z "$(img_on "$NODE")" ] && ok "precondition: $NODE still has no worker image" || bad "$NODE has the worker image already: R2 would not pull"
tenant reg-b
if $K -n $NS wait --for=condition=Ready pod/reg-b --timeout=240s >/dev/null 2>&1; then
    got=$($K -n $NS exec reg-b -c agent -- cat /mnt/s3/shard-00.txt 2>/dev/null)
    [ "$got" = seeded-object-00 ] && ok "reg-b mounted and reads the seed's bytes" || bad "reg-b reads '$got', not seeded-object-00"
else
    bad "reg-b never became Ready: $($K -n $NS get events --field-selector involvedObject.name=reg-b -o jsonpath='{.items[-1:].message}' | cut -c1-200)"
fi
w=$(worker_of reg-b)
[ "$($K -n $WNS get pod "$w" -o jsonpath='{.spec.imagePullSecrets[*].name}' 2>/dev/null)" = regcred ] \
    && ok "worker $w names regcred" || bad "worker '$w' does not name regcred"
[ "$($K -n $WNS get pod "$w" -o jsonpath='{.spec.serviceAccountName}' 2>/dev/null)" = default ] \
    && ok "it runs as default (workers.serviceAccountName unset)" || bad "worker '$w' runs as another account"
[ -n "$(img_on "$NODE")" ] && ok "the worker image is now on $NODE, pulled with regcred" || bad "no worker image on $NODE after the mount"
[ -z "$($K -n $WNS get sa default -o jsonpath='{.imagePullSecrets}')" ] \
    && ok "default in $WNS still carries no pull secret" || bad "default in $WNS gained a pull secret"
gone reg-b

leg R3 "workers.serviceAccountName, and the policy that admits only it"
$K -n $WNS create sa flint-s3-worker --dry-run=client -o yaml | $K apply -f - >/dev/null
chart_up --set workers.prepull.passthrough=false --set workers.serviceAccountName=flint-s3-worker >/dev/null || bad "helm upgrade failed"
rolled || bad "the plugin did not roll"
tenant reg-c
if $K -n $NS wait --for=condition=Ready pod/reg-c --timeout=240s >/dev/null 2>&1; then
    got=$($K -n $NS exec reg-c -c agent -- cat /mnt/s3/shard-00.txt 2>/dev/null)
    [ "$got" = seeded-object-00 ] && ok "reg-c mounted and reads the seed's bytes" || bad "reg-c reads '$got'"
else
    bad "reg-c never became Ready"
fi
w=$(worker_of reg-c)
[ "$($K -n $WNS get pod "$w" -o jsonpath='{.spec.serviceAccountName}' 2>/dev/null)" = flint-s3-worker ] \
    && ok "worker $w runs as flint-s3-worker" || bad "worker '$w' does not run as flint-s3-worker"
[ "$($K -n $WNS get pod "$w" -o jsonpath='{.spec.automountServiceAccountToken}' 2>/dev/null)" = false ] \
    && ok "with no token mounted" || bad "worker '$w' mounts a token"
# The policy, asked as the plugin: its own pod-bound token (which carries
# the node name the policy checks) and the live worker's own spec.
pp=$($K -n $SYS get pods -l app.kubernetes.io/name=flint-s3-csi-node --field-selector spec.nodeName="$NODE" -o jsonpath='{.items[0].metadata.name}')
puid=$($K -n $SYS get pod "$pp" -o jsonpath='{.metadata.uid}')
TOKEN=$($K -n $SYS create token flint-s3-csi-node --bound-object-kind Pod --bound-object-name "$pp" --bound-object-uid "$puid" --duration 10m)
# A kubeconfig with the token ONLY. kind's own carries a client
# certificate, which the API server authenticates BEFORE a bearer token:
# `--token` on top of it still asks as kubernetes-admin (the first run of
# this leg did, and the accepting arm caught it).
$K config view --minify --raw -o jsonpath='{.clusters[0].cluster.certificate-authority-data}' | base64 -d >"$STATE/ca.crt"
kubectl --kubeconfig "$STATE/plugin.config" config set-cluster c \
    --server="$($K config view --minify -o jsonpath='{.clusters[0].cluster.server}')" \
    --certificate-authority="$STATE/ca.crt" --embed-certs >/dev/null
kubectl --kubeconfig "$STATE/plugin.config" config set-credentials plugin --token="$TOKEN" >/dev/null
kubectl --kubeconfig "$STATE/plugin.config" config set-context plugin --cluster=c --user=plugin >/dev/null
kubectl --kubeconfig "$STATE/plugin.config" config use-context plugin >/dev/null
P="kubectl --kubeconfig $STATE/plugin.config"
who=$($P auth whoami -o jsonpath='{.status.userInfo.username} {.status.userInfo.extra.authentication\.kubernetes\.io/node-name[0]}' 2>&1)
[ "$who" = "system:serviceaccount:$SYS:flint-s3-csi-node $NODE" ] \
    && ok "the dry runs ask as the plugin on $NODE: $who" \
    || bad "the dry runs would not ask as the plugin on $NODE: '$who'"
as_plugin() { # sa-or-"" -> the API server's answer to a dry-run create
    $K -n $WNS get pod "$w" -o json | jq --arg sa "$1" '
        del(.status, .metadata.uid, .metadata.resourceVersion, .metadata.creationTimestamp,
            .metadata.managedFields, .metadata.generation)
        | .metadata.name = "s3w-dryrun-r3" | .spec.serviceAccount = null
        | if $sa == "" then del(.spec.serviceAccountName) else .spec.serviceAccountName = $sa end' |
        $P -n $WNS create --dry-run=server -f - 2>&1
}
out=$(as_plugin flint-s3-worker)
echo "$out" | grep -q "created (server dry run)" \
    && ok "the plugin may create this worker as flint-s3-worker (the accepting arm)" \
    || bad "the policy refused the worker's own spec, so the arms below would prove nothing: $(echo "$out" | cut -c1-300)"
for sa in default ""; do
    out=$(as_plugin "$sa")
    echo "$out" | grep -q "must run as the ServiceAccount the chart names" \
        && ok "the same worker as '${sa:-<unset, defaulted>}' is refused by the policy's account rule" \
        || bad "the same worker as '${sa:-<unset>}' was not refused by the account rule: $(echo "$out" | cut -c1-300)"
done
gone reg-c

leg R4 "the prepull pulls the worker image under the node's list"
for n in $(nodes); do docker exec "$n" crictl rmi "$WORKER_IMG" >/dev/null 2>&1; done
for n in $(nodes); do
    [ -z "$(img_on "$n")" ] && ok "precondition: $n has no worker image" || bad "$n still has the worker image"
done
chart_up >/dev/null || bad "helm upgrade failed"
if rolled; then
    ok "the plugin rolled with prepull-passthrough on every node"
    for n in $(nodes); do
        [ -n "$(img_on "$n")" ] && ok "$n has the worker image again, pulled by the prepull" || bad "$n has no worker image after the prepull"
    done
else
    bad "the plugin did not roll: $($K -n $SYS get pods -o wide --no-headers | tr '\n' ';' | cut -c1-300)"
fi

echo
echo "private-registry: $PASS ok, $FAILED BAD"
[ "$FAILED" -eq 0 ]
