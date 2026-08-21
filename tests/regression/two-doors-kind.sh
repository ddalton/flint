#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# TWO DOORS, ONE HUB — the coherence drill.
#
#   tests/regression/two-doors-kind.sh          (KEEP=1 leaves it standing)
#
# flint-lite's whole shape rests on a claim that has never been tested
# end to end: a project is reachable through TWO doors at once — agents
# mount it over NFS, the control plane edits it over HTTP — and both see
# THE SAME TREE AT THE SAME INSTANT. Everything else (the browse API,
# the front-door contract, the idle ladder's "wake on either") assumes
# it.
#
# So: ONE pod holds BOTH doors. It mounts the share through a real
# PersistentVolumeClaim — the path a consumer actually gets — and it
# talks to the file API over HTTP. Every leg writes through one door and
# reads through the other.
#
# No S3. Tier is deliberately OFF (`bucket` absent = the PVC is the
# data), because coherence between the two doors is a property of the
# HUB, not of the tier, and dragging a bucket in would make a local
# drill need credentials it does not need.
#
# ANTI-VACUITY, because this drill has an obvious way to pass while
# testing nothing: if the "NFS mount" were quietly a local directory,
# every write would be visible to itself and every leg would go green.
# Leg 0 therefore proves the mount is a real NFS4 mount to the hub's own
# address before any comparison runs, and leg 6 proves the comparison
# can fail at all.
# ---------------------------------------------------------------------------
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
CHART="$REPO_ROOT/flint-lite-chart"
CARGO_DIR="$REPO_ROOT/spdk-csi-driver"
CLUSTER="${CLUSTER:-two-doors}"
NS="${NS:-doors}"
IMG="${IMG:-flint-lite:two-doors}"
TOKEN="${TOKEN:-two-doors-drill-token}"
FAILED=0

say()  { echo; echo "── $*"; }
pass() { echo "  ✓ $*"; }
fail() { echo "  ✗ $*" >&2; FAILED=$((FAILED+1)); }
k()    { kubectl --context "kind-$CLUSTER" "$@"; }
inpod(){ k -n "$NS" exec consumer -- sh -c "$1" 2>/dev/null; }

cleanup() {
  if [ "${KEEP:-0}" != "1" ]; then
    # Unmount BEFORE the server dies: a dead server under a live mount
    # D-states umount and the node needs a restart to clear it.
    k -n "$NS" exec consumer -- sh -c 'umount -f /mnt/share 2>/dev/null' >/dev/null 2>&1
    kind delete cluster --name "$CLUSTER" >/dev/null 2>&1
  else
    echo; echo "KEEP=1 — cluster '$CLUSTER' left standing"
  fi
}
trap cleanup EXIT

DARCH=$(docker info --format '{{.Architecture}}' 2>/dev/null)
case "$DARCH" in
  aarch64|arm64) TRIPLE=aarch64-unknown-linux-musl; PLATFORM=linux/arm64 ;;
  x86_64|amd64)  TRIPLE=x86_64-unknown-linux-musl;  PLATFORM=linux/amd64 ;;
  *) echo "unrecognized docker arch: $DARCH" >&2; exit 2 ;;
esac

say "building the hub from the WORKING TREE ($TRIPLE)"
(cd "$CARGO_DIR" && cargo zigbuild --release --target "$TRIPLE" --bin flint-pnfs-mds \
   >/tmp/two-doors-build.log 2>&1) || { tail -5 /tmp/two-doors-build.log; exit 1; }
IMGDIR=$(mktemp -d -t two-doors.XXXXXX)
cp "$CARGO_DIR/target/$TRIPLE/release/flint-pnfs-mds" "$IMGDIR/"
printf 'FROM alpine:3.20\nCOPY flint-pnfs-mds /usr/local/bin/flint-pnfs-mds\n' >"$IMGDIR/Dockerfile"
docker build --platform "$PLATFORM" -t "$IMG" "$IMGDIR" >/tmp/two-doors-img.log 2>&1 \
  || { tail -5 /tmp/two-doors-img.log; exit 1; }
rm -rf "$IMGDIR"
pass "image $IMG built"

say "kind cluster '$CLUSTER'"
kind delete cluster --name "$CLUSTER" >/dev/null 2>&1
kind create cluster --name "$CLUSTER" --wait 120s >/dev/null 2>&1 || { echo "kind create failed" >&2; exit 1; }
kind load docker-image "$IMG" --name "$CLUSTER" >/dev/null 2>&1 || { echo "kind load failed" >&2; exit 1; }
pass "cluster up, image loaded"

say "installing the hub — tier OFF, file API ON"
k create ns "$NS" >/dev/null 2>&1
k -n "$NS" create secret generic api-token --from-literal=token="$TOKEN" >/dev/null 2>&1
helm --kube-context "kind-$CLUSTER" install flint-lite "$CHART" -n "$NS" \
  --set image.ref="$IMG" \
  --set persistence.size=2Gi \
  --set monitoring.enabled=true \
  --set monitoring.fileApi.enabled=true \
  --set monitoring.fileApi.tokenSecret=api-token \
  --wait --timeout 5m >/tmp/two-doors-helm.log 2>&1 \
  || { tail -15 /tmp/two-doors-helm.log; echo "helm install failed" >&2; exit 1; }
# TWO ADDRESSES, ON PURPOSE — this is the real topology, not a
# convenience. The consumer-facing Service carries ONLY the NFS port:
# the file API is deliberately never published on it, because a
# LoadBalancer share would then expose a surface that can rewrite any
# file in the project. So agents reach the share through the Service,
# and the control plane reaches the file API at the POD IP — which is
# exactly how the operator polls /status. A drill that put both doors
# on one address would be testing a deployment nobody ships.
HUB_IP=$(k -n "$NS" get svc flint-lite -o jsonpath='{.spec.clusterIP}')
POD_IP=$(k -n "$NS" get pods -l app=flint-lite -o jsonpath='{.items[0].status.podIP}')
pass "NFS door: Service $HUB_IP:2049   REST door: pod $POD_IP:8080"

# ── the PVC, and the pod that holds BOTH doors ───────────────────────
# A STATIC NFS PersistentVolume pointing at the hub's own Service. This
# is the consumer path — a pod gets a PVC, not a mount command — and it
# is what makes the drill about the product rather than about `mount`.
say "binding the share through a real PVC"
cat <<YAML | k apply -f - >/dev/null 2>&1
apiVersion: v1
kind: PersistentVolume
metadata: {name: share-pv}
spec:
  capacity: {storage: 2Gi}
  accessModes: [ReadWriteMany]
  persistentVolumeReclaimPolicy: Retain
  storageClassName: ""
  mountOptions: [vers=4.1, nconnect=2, hard, timeo=100]
  nfs:
    # The export is the SERVER ROOT. `/data/exports` is a path INSIDE
    # the container and the server refuses it with NFS4ERR_NOENT — a
    # mistake the operator chart's NOTES shipped until 1.32.0.
    server: $HUB_IP
    path: /
---
apiVersion: v1
kind: PersistentVolumeClaim
metadata: {name: share-pvc, namespace: $NS}
spec:
  accessModes: [ReadWriteMany]
  storageClassName: ""
  volumeName: share-pv
  resources: {requests: {storage: 2Gi}}
---
apiVersion: v1
kind: Pod
metadata: {name: consumer, namespace: $NS}
spec:
  restartPolicy: Never
  containers:
  - name: c
    image: alpine:3.20
    command: ["sh","-c","apk add --no-cache curl >/dev/null 2>&1; sleep 100000"]
    volumeMounts: [{name: share, mountPath: /mnt/share}]
  volumes:
  - name: share
    persistentVolumeClaim: {claimName: share-pvc}
YAML
for i in $(seq 1 40); do
  [ "$(k -n "$NS" get pod consumer -o jsonpath='{.status.phase}' 2>/dev/null)" = "Running" ] && break
  sleep 5
done
if [ "$(k -n "$NS" get pod consumer -o jsonpath='{.status.phase}' 2>/dev/null)" != "Running" ]; then
  k -n "$NS" describe pod consumer 2>&1 | grep -A6 Events | tail -8
  echo "consumer pod never ran — the PVC mount failed" >&2; exit 1
fi
sleep 10
pass "consumer pod Running with the PVC mounted"

API="curl -s --max-time 20 -H 'Authorization: Bearer $TOKEN'"

# ── LEG 0 — ANTI-VACUITY: is that mount REAL? ────────────────────────
# If /mnt/share were a local directory, every leg below would pass while
# proving nothing at all: a write is always visible to itself.
say "[leg 0] ANTI-VACUITY — the mount is a real NFS4 mount to THIS hub"
MNT=$(inpod "mount | grep ' /mnt/share '")
echo "  $MNT"
if echo "$MNT" | grep -q "type nfs4"; then pass "filesystem type is nfs4"; else fail "NOT an nfs4 mount — every comparison below would be vacuous"; fi
if echo "$MNT" | grep -q "$HUB_IP"; then pass "server is the hub's ClusterIP $HUB_IP"; else fail "mounted something that is not this hub"; fi
REST_UP=$(inpod "$API -o /dev/null -w '%{http_code}' http://$POD_IP:8080/status")
if [ "$REST_UP" = "200" ]; then pass "the REST door answers 200 at the pod IP"; else fail "REST door returned $REST_UP — the other door is not open"; fi
# And the file API must NOT be reachable on the consumer Service. If it
# were, every NFS consumer could rewrite the whole project.
ON_SVC=$(inpod "$API -o /dev/null -w '%{http_code}' --max-time 6 http://$HUB_IP:8080/status")
if [ "$ON_SVC" = "200" ]; then fail "the file API is exposed on the CONSUMER Service — any mounter could rewrite the project"; else pass "the file API is NOT on the consumer Service (got '$ON_SVC')"; fi

# ── LEG 1 — REST write → NFS read ────────────────────────────────────
say "[leg 1] a file written through the REST door is readable through the mount"
inpod "head -c 200000 /dev/urandom > /tmp/a.bin"
SRC=$(inpod "md5sum /tmp/a.bin | cut -d' ' -f1")
RC=$(inpod "$API -o /dev/null -w '%{http_code}' -X PUT --data-binary @/tmp/a.bin 'http://$POD_IP:8080/files/content?path=from-rest.bin'")
VIA_NFS=$(inpod "md5sum /mnt/share/from-rest.bin 2>/dev/null | cut -d' ' -f1")
echo "  PUT $RC   src=$SRC   via NFS=$VIA_NFS"
if [ "$RC" = "201" ] && [ "$SRC" = "$VIA_NFS" ] && [ -n "$VIA_NFS" ]; then
  pass "byte-identical across the doors"
else fail "REST write did not appear intact on the mount"; fi

# ── LEG 2 — NFS write → REST read ────────────────────────────────────
say "[leg 2] a file written through the mount is readable through the REST door"
inpod "head -c 150000 /dev/urandom > /mnt/share/from-nfs.bin"
SRC2=$(inpod "md5sum /mnt/share/from-nfs.bin | cut -d' ' -f1")
CODE2=$(inpod "$API -o /tmp/b.bin -w '%{http_code}' 'http://$POD_IP:8080/files/content?path=from-nfs.bin'")
VIA_REST=$(inpod "md5sum /tmp/b.bin | cut -d' ' -f1")
echo "  GET $CODE2   src=$SRC2   via REST=$VIA_REST"
if [ "$CODE2" = "200" ] && [ "$SRC2" = "$VIA_REST" ]; then
  pass "byte-identical across the doors"
else fail "NFS write did not come back intact through the REST door"; fi

# ── LEG 3 — the two doors agree on METADATA, not just bytes ──────────
say "[leg 3] both doors report the same size for the same file"
NFS_SZ=$(inpod "stat -c %s /mnt/share/from-rest.bin")
# Parse the listing as JSON on the host — a grep chain over JSON reads
# whichever field happens to sit next to the name, which is how this
# leg first reported an empty size against a perfectly good listing.
LISTING=$(inpod "$API 'http://$POD_IP:8080/files?path='")
REST_SZ=$(printf '%s' "$LISTING" | python3 -c "
import json,sys
try: d=json.load(sys.stdin)
except Exception: print(''); raise SystemExit
print(next((e.get('size') for e in d.get('entries',[]) if e.get('name')=='from-rest.bin'), ''))
")
echo "  NFS stat=$NFS_SZ   REST listing=$REST_SZ"
if [ -n "$NFS_SZ" ] && [ "$NFS_SZ" = "$REST_SZ" ]; then pass "sizes agree ($NFS_SZ)"; else fail "size disagreement: NFS $NFS_SZ vs REST $REST_SZ"; fi

# ── LEG 4 — a directory made through one door is usable in the other ─
say "[leg 4] a directory created over REST is writable through the mount"
MK=$(inpod "$API -o /dev/null -w '%{http_code}' -X POST -H 'Content-Type: application/json' -d '{\"path\":\"shared-dir\"}' http://$POD_IP:8080/files/folder")
W=$(inpod "echo hello-from-nfs > /mnt/share/shared-dir/note.txt && echo ok")
BACK=$(inpod "$API 'http://$POD_IP:8080/files/content?path=shared-dir/note.txt'")
echo "  mkdir $MK   nfs write=$W   read back over REST='$BACK'"
if [ "$MK" = "201" ] && [ "$W" = "ok" ] && [ "$BACK" = "hello-from-nfs" ]; then
  pass "directory and its contents cross both ways"
else fail "the directory did not cross cleanly"; fi

# ── LEG 5 — a delete through one door is a delete in the other ───────
say "[leg 5] deleting over REST removes it from the mount"
DEL=$(inpod "$API -o /dev/null -w '%{http_code}' -X DELETE 'http://$POD_IP:8080/files/content?path=from-nfs.bin'")
GONE=$(inpod "test -e /mnt/share/from-nfs.bin && echo STILL_THERE || echo gone")
echo "  DELETE $DEL   on the mount: $GONE"
if [ "$DEL" = "200" ] && [ "$GONE" = "gone" ]; then pass "the delete crossed"; else fail "deleted over REST but still visible on the mount"; fi

# ── LEG 6 — ANTI-VACUITY: can any of this FAIL? ──────────────────────
# Every leg above is a string comparison, and a comparison that cannot
# fail is decoration. Change the file under one door only and require
# the SAME comparison to report a mismatch.
say "[leg 6] ANTI-VACUITY — the comparison can fail"
inpod "printf 'tampered' >> /mnt/share/from-rest.bin"
AFTER=$(inpod "md5sum /mnt/share/from-rest.bin | cut -d' ' -f1")
inpod "$API -o /tmp/c.bin 'http://$POD_IP:8080/files/content?path=from-rest.bin'" >/dev/null
REST_AFTER=$(inpod "md5sum /tmp/c.bin | cut -d' ' -f1")
echo "  original=$SRC   after tamper (NFS)=$AFTER   (REST)=$REST_AFTER"
if [ "$AFTER" != "$SRC" ]; then pass "the checksum moved, so leg 1's equality was a real test"; else fail "tampering did not change the checksum — the comparison is broken"; fi
if [ "$AFTER" = "$REST_AFTER" ]; then pass "and BOTH doors see the tampered version — still coherent"; else fail "the doors disagree after a write: $AFTER vs $REST_AFTER"; fi

echo
if [ "$FAILED" -gt 0 ]; then echo "TWO DOORS: $FAILED leg(s) FAILED"; exit 1; fi
echo "TWO DOORS: all legs passed — one pod, one hub, two doors, one tree"
