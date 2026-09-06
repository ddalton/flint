#!/usr/bin/env bash
# Stand the walgit arm up on the campaign's cluster, beside forge's rig.
#
#   BUCKET=... WPREFIX=walgit/<stamp> ./forge/e2e/walgit/deploy-walgit.sh
#
# walgit publishes no image and its Containerfile compiles a Rust
# workspace plus a React UI, which a laptop's Docker VM could not finish
# (the link was OOM-killed on 2026-09-05). So the image is built ON THE
# CLUSTER: a docker-in-docker pod on one worker, its data-root on the
# NVMe prep-nodes.sh mounted, builds straight from walgit's git URL at
# the pinned commit; the image is saved to a tarball on that NVMe and
# imported into the node's containerd; the walgit Deployment is pinned
# to that node with imagePullPolicy Never. No registry, no credential
# leaves the machine.
#
# Two things the first campaign learned, 2026-09-05. The import lands in
# containerd's store on the node's 8 GiB ROOT disk (prep-nodes.sh moves
# pod volumes, not images): walgit's image is 949 MB, and importing it
# put the node under DiskPressure, which rejects every new pod for the
# kubelet's five-minute transition period — including the import pod of
# the NEXT run. Import once; if a re-run is needed, apply the rendered
# Deployment by hand rather than re-importing. And a `kubectl exec`
# readiness probe on dockerd needs `timeoutSeconds` above the default 1 s.
#
# Knobs: WALGIT_REF (the pinned commit) BUILDER_NODE (default: the second worker) KEEP_BUILDER
set -euo pipefail
here=$(cd "$(dirname "$0")" && pwd)
: "${BUCKET:?BUCKET is required}"
: "${WPREFIX:?WPREFIX is required (walgit/<stamp>)}"
# The FULL commit hash: BuildKit's git context refuses a short one ("repository does not contain ref e5295e6").
WALGIT_REF=${WALGIT_REF:-e5295e6ee45f5267c661f8bbd27ed0a07e55e7db}
WALGIT_TAG=${WALGIT_REF:0:7}
WALGIT_REPO=${WALGIT_REPO:-https://github.com/tobi/walgit.git}
WALGIT_IMAGE="walgit:$WALGIT_TAG"
NS=agents
LOG=${LOG:-$here/results/build-walgit-$WALGIT_TAG.log}; mkdir -p "$(dirname "$LOG")"

workers=$(kubectl get nodes -l '!node-role.kubernetes.io/control-plane' -o jsonpath='{.items[*].metadata.name}')
set -- $workers
BUILDER_NODE=${BUILDER_NODE:-${2:-$1}}
echo "── walgit $WALGIT_TAG ($WALGIT_REF) on node $BUILDER_NODE (workers: $workers) ──"

kubectl create namespace "$NS" --dry-run=client -o yaml | kubectl apply -f - >/dev/null

# ── 1. the builder pod ─────────────────────────────────────────────
cat <<YAML | kubectl apply -f - >/dev/null
apiVersion: v1
kind: Pod
metadata: { name: walgit-builder, namespace: $NS, labels: { role: walgit-builder } }
spec:
  nodeName: $BUILDER_NODE
  restartPolicy: Never
  containers:
    - name: dind
      image: docker:27-dind
      securityContext: { privileged: true }
      env: [ { name: DOCKER_TLS_CERTDIR, value: "" } ]
      args: ["--host=unix:///var/run/docker.sock"]
      volumeMounts:
        - { name: data, mountPath: /var/lib/docker }
      readinessProbe:
        exec: { command: ["docker", "info"] }
        periodSeconds: 3
        timeoutSeconds: 10
  volumes:
    # An emptyDir, which prep-nodes.sh has put on the NVMe. A hostPath
    # under /mnt/pods looked equivalent and was not: dockerd's managed
    # containerd could not mkdir under it ("no such file or directory")
    # on the bind-mounted tree, 2026-09-05.
    - name: data
      emptyDir: {}
YAML
kubectl -n "$NS" wait --for=condition=Ready pod/walgit-builder --timeout=5m
echo "  builder ready: $(kubectl -n "$NS" exec walgit-builder -- docker version --format '{{.Server.Version}}')"

# ── 2. build from the git URL at the pinned commit ────────────────
if kubectl -n "$NS" exec walgit-builder -- docker image inspect "$WALGIT_IMAGE" >/dev/null 2>&1; then
    echo "  image $WALGIT_IMAGE already built on the node"
else
    echo "  building $WALGIT_IMAGE from $WALGIT_REPO#$WALGIT_REF (log: $LOG) …"
    t0=$(date +%s)
    kubectl -n "$NS" exec walgit-builder -- docker build --progress=plain -t "$WALGIT_IMAGE" \
        --build-arg "WALGIT_BUILD_SHA=$WALGIT_TAG" -f Containerfile "$WALGIT_REPO#$WALGIT_REF" > "$LOG" 2>&1 \
        || { echo "  BUILD FAILED — tail of $LOG:"; tail -30 "$LOG"; exit 1; }
    echo "  built in $(( $(date +%s) - t0 )) s"
fi
kubectl -n "$NS" exec walgit-builder -- docker image ls "$WALGIT_IMAGE" --format '  {{.Repository}}:{{.Tag}} {{.Size}} {{.ID}}'

# ── 3. save into the builder's emptyDir and import it into the node's containerd ──
# The emptyDir's host path is deterministic from the pod's UID, so the
# import pod (hostPID, nsenter into the node) reads the tarball where it
# lies — nothing crosses the network, no registry, no credential.
kubectl -n "$NS" exec walgit-builder -- sh -c "docker save $WALGIT_IMAGE -o /var/lib/docker/walgit-$WALGIT_TAG.tar && ls -la /var/lib/docker/walgit-$WALGIT_TAG.tar"
BUILDER_UID=$(kubectl -n "$NS" get pod walgit-builder -o jsonpath='{.metadata.uid}')
TAR="/var/lib/kubelet/pods/$BUILDER_UID/volumes/kubernetes.io~empty-dir/data/walgit-$WALGIT_TAG.tar"
kubectl -n "$NS" delete pod walgit-import --ignore-not-found --wait=true >/dev/null 2>&1
cat <<YAML | kubectl apply -f - >/dev/null
apiVersion: v1
kind: Pod
metadata: { name: walgit-import, namespace: $NS }
spec:
  nodeName: $BUILDER_NODE
  hostPID: true
  restartPolicy: Never
  containers:
    - name: import
      image: busybox:1.36
      securityContext: { privileged: true }
      command: ["nsenter", "-t", "1", "-m", "-u", "-i", "-n", "--",
                "ctr", "-n", "k8s.io", "images", "import", "$TAR"]
YAML
for i in $(seq 1 60); do
    ph=$(kubectl -n "$NS" get pod walgit-import -o jsonpath='{.status.phase}' 2>/dev/null)
    [ "$ph" = Succeeded ] && break
    [ "$ph" = Failed ] && { echo "  IMPORT FAILED:"; kubectl -n "$NS" logs walgit-import | tail -20; exit 1; }
    sleep 2
done
echo "  imported: $(kubectl -n "$NS" logs walgit-import | tail -1)"
kubectl -n "$NS" delete pod walgit-import --wait=false >/dev/null 2>&1

# ── 4. the Deployment, pinned to the node that holds the image ─────
# A first deploy has no secret yet; under `set -e` a failing substitution
# inside the assignment would end the script here (it did, 2026-09-05).
existing=$(kubectl -n "$NS" get secret walgit-token -o jsonpath='{.data.WALGIT_TOKEN_AGENT}' 2>/dev/null | base64 -d || true)
WALGIT_TOKEN=${WALGIT_TOKEN:-$existing}
[ -n "$WALGIT_TOKEN" ] || WALGIT_TOKEN=$(openssl rand -hex 24)
BUCKET="$BUCKET" WPREFIX="$WPREFIX" WALGIT_IMAGE="docker.io/library/$WALGIT_IMAGE" WALGIT_NODE="$BUILDER_NODE" WALGIT_TOKEN="$WALGIT_TOKEN" \
    envsubst '$BUCKET $WPREFIX $WALGIT_IMAGE $WALGIT_NODE $WALGIT_TOKEN' < "$here/walgit.yaml.tpl" | kubectl apply -f -
kubectl -n "$NS" rollout restart deploy/walgit >/dev/null 2>&1 || true
kubectl -n "$NS" rollout status deploy/walgit --timeout=3m
pod=$(kubectl -n "$NS" get pod -l app=walgit -o jsonpath='{.items[0].metadata.name}')
echo "  $(kubectl -n "$NS" exec "$pod" -- walgit --version 2>/dev/null | head -1) on $(kubectl -n "$NS" get pod "$pod" -o jsonpath='{.spec.nodeName}')"
[ "${KEEP_BUILDER:-no}" = yes ] || kubectl -n "$NS" delete pod walgit-builder --wait=false >/dev/null 2>&1
echo
echo "deployed: walgit $WALGIT_TAG at http://walgit.$NS.svc:8080/acme/<repo>.git, bucket $BUCKET, prefix $WPREFIX"
echo "token:    kubectl -n $NS get secret walgit-token -o jsonpath='{.data.WALGIT_TOKEN_AGENT}' | base64 -d"
