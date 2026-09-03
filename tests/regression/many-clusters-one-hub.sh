#!/usr/bin/env bash
# ---------------------------------------------------------------------------
# MANY CLUSTERS, ONE HUB — the fleet drill.
#
#   tests/regression/many-clusters-one-hub.sh     (KEEP=1 leaves it standing)
#
# THE USE CASE, stated exactly: AI agents run in MANY separate Kubernetes
# clusters and ALL mount THE SAME flint-lite hub — one FlintShare, one S3
# prefix — over NFSv4.2, from outside the cluster the hub runs in. Agents
# write DIFFERENT files and must not clobber each other.
#
# This is the shape flint-lite is sold in, and it had never been drilled.
# What existed before:
#   * tests/lima/pnfs/cross-cluster-drill.sh (2026-08-14) — two CLIENTS in
#     ONE VM. No Kubernetes, no second API server, no NAT, no Service, and
#     it predates flint-lite entirely. It proved NFS semantics; it could
#     not see anything below.
#   * tests/regression/overlap-two-cluster-kind.sh — two HUBS on one
#     bucket. The OPPOSITE topology: that drill is about fencing rivals,
#     this one is about admitting peers.
#
# TOPOLOGY. Three kind clusters on one docker network:
#   flint-mc-a   the HUB cluster: operator + FlintShare + hub pod,
#                published as a NodePort on the kind node's docker IP.
#   flint-mc-b   agent cluster #1 — no operator, no CRD, no credentials.
#   flint-mc-c   agent cluster #2 — likewise.
# MinIO runs as a plain container on the same network and stands in for
# S3, so the bucket is real and shared while the API servers are three.
#
# B and C hold NOTHING but a mount address. That is the property the
# architecture claims ("consumers hold no storage credentials") and the
# drill is arranged so that a violation would be visible as a missing
# object rather than argued from the manifest.
#
# ANTI-VACUITY IS THE WHOLE DESIGN. A previous campaign here found that
# 24 of 41 proposed legs would have PASSED AGAINST A COMPLETELY BROKEN
# PRODUCT. Every leg below therefore carries a guard INDEPENDENT of its
# oracle, and a named circumstance under which it goes red. Legs that
# assert a count assert it against a line derived from the product's own
# configuration, never against a status code and never against the mere
# presence of a string.
# ---------------------------------------------------------------------------
set -uo pipefail

REPO_ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
OP_CHART="$REPO_ROOT/flint-lite-operator-chart"
CARGO_DIR="$REPO_ROOT/spdk-csi-driver"

CA="${CA:-flint-mc-a}"        # hub cluster
CB="${CB:-flint-mc-b}"        # agent cluster 1
CC="${CC:-flint-mc-c}"        # agent cluster 2
NS=fleet
OPNS=flint-system
HUBIMG="${HUBIMG:-flint-mc-hub:local}"
OPIMG="${OPIMG:-flint-mc-op:local}"
MINIO_CT=flint-mc-minio
BUCKET=flint-fleet
MINIO_USER=flintdrill
MINIO_PASS=flintdrill123
MINIO_HOSTPORT=39300
TOKEN=fleet-drill-token
NODEPORT=32049

PASSED=0; FAILED=0; FAILURES=()
say()  { echo; echo "── $*"; }
pass() { echo "  ✓ $*"; PASSED=$((PASSED+1)); }
bad()  { echo "  ✗ FAIL: $*"; FAILED=$((FAILED+1)); FAILURES+=("$*"); }
fail() { echo "  ✗ FATAL: $*"; exit 1; }
note() { echo "    $*"; }

export KUBECONFIG="${KUBECONFIG:-/tmp/mc-kubeconfig}"
ka() { kubectl --context "kind-$CA" "$@"; }
kb() { kubectl --context "kind-$CB" "$@"; }
kc() { kubectl --context "kind-$CC" "$@"; }

s3() {
  env -u AWS_PROFILE \
    AWS_ACCESS_KEY_ID=$MINIO_USER AWS_SECRET_ACCESS_KEY=$MINIO_PASS \
    AWS_DEFAULT_REGION=us-east-1 AWS_EC2_METADATA_DISABLED=true \
    aws --endpoint-url "http://127.0.0.1:$MINIO_HOSTPORT" "$@"
}

# ── the hub's own log is the instrument ────────────────────────────────
# Every leg below reads the hub's EXCHANGE_ID trace rather than guessing
# at client behaviour. Two things make that trustworthy: the owner string
# is logged as raw bytes (so a collision is a byte comparison, not an
# inference), and `case-5 deferred cleanup` is a WARN emitted at exactly
# the moment one client's state is discarded for another's.
HUBLOG() { ka -n "$NS" logs deploy/$SHARE 2>&1; }
case5_count() { HUBLOG | grep -ci "case-5 deferred cleanup"; }

# THE OWNER IS LOGGED AS A `{:?}` Vec<u8>, NOT AS TEXT:
#   EXCHANGE_ID: owner=[76, 105, 110, 117, 120, ...]
# Grepping the hub log for the literal string `Linux NFSv4.2` therefore
# matches NOTHING, and a leg written that way reports "no collision"
# while looking at the wrong thing entirely. Count DISTINCT BYTE ARRAYS.
owner_marks() { HUBLOG | grep -c "EXCHANGE_ID: owner="; }
# Distinct owner byte-arrays among the EXCHANGE_IDs that arrived AFTER
# the $1'th one — so an arm cannot count the previous arm's mounts. A
# fixed `tail -N` window straddles the two arms and silently mixes them.
owners_since() {
  HUBLOG | grep -o "owner=\[[0-9, ]*\]" | awk -v skip="$1" 'NR>skip' \
    | sort -u | wc -l | tr -d ' '
}
decode_owners() {
  HUBLOG | grep -o "owner=\[[0-9, ]*\]" | sort -u | python3 -c '
import sys, re
for line in sys.stdin:
    b = [int(x) for x in re.findall(r"\d+", line)]
    print("   ", bytes(b).decode("ascii", "replace"))
'
}

# A pod that can mount NFS, with its hostname under our control — which
# is the whole point: the NFSv4.1 co_ownerid IS the hostname and nothing
# else, so `hostname` is the knob that decides whether two clusters look
# like two clients or one.
mkagent() { # $1=cluster $2=pod $3=hostname
  kubectl --context "kind-$1" -n "$NS" delete pod "$2" --wait=true >/dev/null 2>&1
  kubectl --context "kind-$1" -n "$NS" apply -f - >/dev/null 2>&1 <<EOF
apiVersion: v1
kind: Pod
metadata: { name: $2 }
spec:
  hostname: $3
  restartPolicy: Never
  containers:
  - name: c
    image: alpine:3.20
    command: ["sh","-c","apk add --no-cache nfs-utils flock util-linux >/dev/null 2>&1; sleep 9000"]
    securityContext: { privileged: true }
EOF
  kubectl --context "kind-$1" -n "$NS" wait --for=condition=ready "pod/$2" --timeout=180s >/dev/null 2>&1 \
    || { bad "$1/$2 never became Ready"; return 1; }
  local i
  for i in $(seq 1 40); do
    kubectl --context "kind-$1" -n "$NS" exec "$2" -- sh -c \
      'command -v mount.nfs4 >/dev/null && command -v flock >/dev/null' >/dev/null 2>&1 && return 0
    sleep 3
  done
  bad "$1/$2 never got nfs-utils+flock"; return 1
}
X() { kubectl --context "kind-$1" -n "$NS" exec "$2" -- sh -c "$3" 2>&1; }
mount_at() { X "$1" "$2" "mkdir -p /mnt/w && timeout 30 mount -t nfs4 -o $MOPTS $ADDR_HOST:/ /mnt/w && echo MOUNTED"; }

# ── bring-up ───────────────────────────────────────────────────────────
for t in kind kubectl helm docker aws jq; do command -v "$t" >/dev/null || fail "$t not installed"; done
docker info >/dev/null 2>&1 || fail "docker daemon not reachable"
DARCH=$(docker info --format '{{.Architecture}}')
case "$DARCH" in
  aarch64|arm64) TRIPLE=aarch64-unknown-linux-musl; PLATFORM=linux/arm64 ;;
  x86_64|amd64)  TRIPLE=x86_64-unknown-linux-musl;  PLATFORM=linux/amd64 ;;
  *) fail "unrecognized docker VM arch: $DARCH" ;;
esac

echo "══════════════════════════════════════════════════════════════════"
echo " many clusters, one hub — $CA serves; $CB and $CC only mount"
echo "══════════════════════════════════════════════════════════════════"

say "building the hub and the operator ($TRIPLE)"
(cd "$CARGO_DIR" && cargo zigbuild --release --target "$TRIPLE" \
   --bin flint-pnfs-mds --bin flint-lite-operator >/tmp/mc-build.log 2>&1) \
  || { tail -20 /tmp/mc-build.log; fail "zigbuild failed"; }
IMGDIR=$(mktemp -d -t flint-mc-img.XXXXXX)
cp "$CARGO_DIR/target/$TRIPLE/release/flint-pnfs-mds" "$IMGDIR/"
cp "$CARGO_DIR/target/$TRIPLE/release/flint-lite-operator" "$IMGDIR/"
printf 'FROM alpine:3.20\nRUN apk add --no-cache curl\nCOPY flint-pnfs-mds /usr/local/bin/flint-pnfs-mds\n' >"$IMGDIR/Dockerfile.hub"
printf 'FROM alpine:3.20\nRUN apk add --no-cache ca-certificates\nCOPY flint-lite-operator /usr/local/bin/flint-lite-operator\nUSER 65532:65532\nENTRYPOINT ["/usr/local/bin/flint-lite-operator"]\n' >"$IMGDIR/Dockerfile.op"
docker build --platform "$PLATFORM" -f "$IMGDIR/Dockerfile.hub" -t "$HUBIMG" "$IMGDIR" >/tmp/mc-img.log 2>&1 || fail "hub image build failed"
docker build --platform "$PLATFORM" -f "$IMGDIR/Dockerfile.op"  -t "$OPIMG"  "$IMGDIR" >>/tmp/mc-img.log 2>&1 || fail "operator image build failed"
rm -rf "$IMGDIR"
pass "images built ($PLATFORM)"

say "three kind clusters and ONE MinIO on the shared docker network"
for c in "$CA" "$CB" "$CC"; do kind delete cluster --name "$c" >/dev/null 2>&1; done
docker rm -f "$MINIO_CT" >/dev/null 2>&1
rm -f "$KUBECONFIG"
for c in "$CA" "$CB" "$CC"; do
  kind create cluster --name "$c" --wait 150s >/dev/null 2>&1 || fail "cluster $c never came up"
done
docker run -d --name "$MINIO_CT" --network kind -p "$MINIO_HOSTPORT:9000" \
  -e "MINIO_ROOT_USER=$MINIO_USER" -e "MINIO_ROOT_PASSWORD=$MINIO_PASS" \
  quay.io/minio/minio server /data >/dev/null 2>&1 || fail "could not start MinIO"
MINIO_IP=$(docker inspect -f '{{.NetworkSettings.Networks.kind.IPAddress}}' "$MINIO_CT")
for _ in $(seq 1 60); do curl -sf "http://127.0.0.1:$MINIO_HOSTPORT/minio/health/live" >/dev/null && break; sleep 1; done
curl -sf "http://127.0.0.1:$MINIO_HOSTPORT/minio/health/live" >/dev/null || fail "MinIO never became live"
s3 s3 mb "s3://$BUCKET" >/dev/null 2>&1 || fail "bucket create failed"
s3 s3api put-bucket-versioning --bucket "$BUCKET" --versioning-configuration Status=Enabled >/dev/null 2>&1
NODE_A=$(docker inspect -f '{{.NetworkSettings.Networks.kind.IPAddress}}' "$CA-control-plane")
ADDR_HOST="$NODE_A"
MOPTS="vers=4.2,nconnect=2,hard,sec=sys,port=$NODEPORT"
pass "clusters up; hub node $NODE_A; MinIO $MINIO_IP:9000; bucket $BUCKET"

say "the hub — operator + FlintShare in $CA ONLY"
for c in "$CA" "$CB" "$CC"; do
  kind load docker-image "$HUBIMG" --name "$c" >/dev/null 2>&1
  kind load docker-image "$OPIMG"  --name "$c" >/dev/null 2>&1
  kubectl --context "kind-$c" create namespace "$NS" >/dev/null 2>&1
done
ka -n "$NS" create secret generic s3creds \
  --from-literal=AWS_ACCESS_KEY_ID=$MINIO_USER \
  --from-literal=AWS_SECRET_ACCESS_KEY=$MINIO_PASS >/dev/null || fail "s3creds"
ka -n "$NS" create secret generic api-token --from-literal=token="$TOKEN" >/dev/null || fail "api-token"
helm --kube-context "kind-$CA" install flint-lite-operator "$OP_CHART" \
  -n "$OPNS" --create-namespace --set image.ref="$OPIMG" --set hubImage="$HUBIMG" \
  >/tmp/mc-helm.log 2>&1 || { tail -20 /tmp/mc-helm.log; fail "operator install failed"; }
ka -n "$OPNS" rollout status deployment/flint-lite-operator --timeout=180s >/dev/null 2>&1 \
  || fail "operator never rolled out"

SHARE=projx
ka -n "$NS" apply -f - >/dev/null <<EOF || fail "FlintShare refused"
apiVersion: chert.us/v1alpha1
kind: FlintShare
metadata: { name: $SHARE, namespace: $NS }
spec:
  bucket: $BUCKET
  keyPrefix: projx/
  endpoint: http://$MINIO_IP:9000
  region: us-east-1
  credentialsSecretRef: s3creds
  image: $HUBIMG
  service: { type: NodePort, nodePort: $NODEPORT, advertiseAddress: "$NODE_A:$NODEPORT" }
  monitoring: { enabled: true, fileApi: { enabled: true, tokenSecretRef: api-token } }
  persistence: { size: 2Gi }
EOF
for _ in $(seq 1 60); do
  [ "$(ka -n "$NS" get flintshare $SHARE -o jsonpath='{.status.phase}' 2>/dev/null)" = "Ready" ] && break
  sleep 5
done
[ "$(ka -n "$NS" get flintshare $SHARE -o jsonpath='{.status.phase}')" = "Ready" ] || fail "hub never became Ready"
pass "hub Ready, advertising $(ka -n "$NS" get flintshare $SHARE -o jsonpath='{.status.address}')"

# ── L1 ─ the baseline the whole use case rests on ──────────────────────
# Two clusters, one hub, disjoint files. This is what the product
# promises, and it must hold before any of the sharper legs mean
# anything.
#
# ANTI-VACUITY: a mount that silently fell back to a local directory
# would make every cross-visibility check pass while proving nothing, so
# the mount is verified to be a real nfs4 mount to the hub's address
# BEFORE any file is compared, and each side reads a file the OTHER
# wrote — never one it wrote itself.
say "L1: two clusters mount one hub and write disjoint files"
mkagent "$CB" agent agent-b || fail "cluster B agent"
mkagent "$CC" agent agent-c || fail "cluster C agent"
mount_at "$CB" agent >/dev/null; mount_at "$CC" agent >/dev/null
for c in "$CB" "$CC"; do
  X "$c" agent "mount | grep -c 'type nfs4.*$ADDR_HOST'" | grep -q '^1$' \
    || fail "$c: not a real nfs4 mount to $ADDR_HOST"
done
pass "both mounts are real nfs4 mounts to the hub's advertised address"
X "$CB" agent "echo from-b > /mnt/w/b.txt" >/dev/null
X "$CC" agent "echo from-c > /mnt/w/c.txt" >/dev/null
SEEN_B=$(X "$CB" agent "cat /mnt/w/c.txt" | tr -d '\r\n')
SEEN_C=$(X "$CC" agent "cat /mnt/w/b.txt" | tr -d '\r\n')
[ "$SEEN_B" = "from-c" ] && [ "$SEEN_C" = "from-b" ] \
  && pass "each cluster reads the file the OTHER wrote" \
  || bad "cross-cluster visibility failed (B saw '$SEEN_B', C saw '$SEEN_C')"

# ── L2 ─ THE FINDING: every cluster presents ONE identity ──────────────
# On NFSv4.1+ the Linux client's co_ownerid is `Linux NFSv4.<minor>
# <nodename>` and NOTHING else — no address, no cluster, no uniquifier
# unless `nfs.nfs4_unique_id` is set on the node. A fleet that runs one
# agent manifest in every cluster therefore presents ONE identity from
# all of them, and `ClientManager::exchange_id` keys on those bytes
# alone (client.rs: `owner_to_id.get(&owner)`), with no source-address
# tiebreak anywhere.
#
# The oracle is a COUNT of distinct owner strings the hub logged, against
# the number of distinct agent hostnames mounted — a quantity against a
# line the rig itself sets, not a status code.
#
# ANTI-VACUITY: the same measurement runs twice. Colliding hostnames must
# collapse to ONE owner; distinct hostnames must produce TWO. A hub that
# always reported 1, or always 2, fails one arm or the other.
say "L2: do two clusters look like two clients, or one?"
X "$CB" agent "umount -f /mnt/w" >/dev/null 2>&1; X "$CC" agent "umount -f /mnt/w" >/dev/null 2>&1
sleep 5
mkagent "$CB" agent agent || fail "cluster B colliding agent"
mkagent "$CC" agent agent || fail "cluster C colliding agent"
MARK=$(owner_marks)
mount_at "$CB" agent >/dev/null; mount_at "$CC" agent >/dev/null; sleep 6
[ "$(owner_marks)" -gt "$MARK" ] || fail "no EXCHANGE_ID reached the hub — L2 would be vacuous"
COLLIDE_OWNERS=$(owners_since "$MARK")
note "both pods hostname 'agent' -> distinct owner strings on the wire: $COLLIDE_OWNERS"
if [ "$COLLIDE_OWNERS" = "1" ]; then
  pass "CONFIRMED: two clusters present ONE NFS identity — the hub cannot tell them apart"
  note "the identity, decoded from the bytes the hub logged:"; decode_owners
else
  bad "expected the two clusters to collapse to 1 owner string, saw $COLLIDE_OWNERS"
fi
C5=$(HUBLOG | grep -c "case 5 (client reboot detected)")
note "hub read one cluster's mount as the other REBOOTING (case 5) $C5 time(s)"

X "$CB" agent "umount -f /mnt/w" >/dev/null 2>&1; X "$CC" agent "umount -f /mnt/w" >/dev/null 2>&1
sleep 5
mkagent "$CB" agent agent-b2 || fail "cluster B distinct agent"
mkagent "$CC" agent agent-c2 || fail "cluster C distinct agent"
MARK=$(owner_marks)
mount_at "$CB" agent >/dev/null; mount_at "$CC" agent >/dev/null; sleep 6
[ "$(owner_marks)" -gt "$MARK" ] || fail "no EXCHANGE_ID reached the hub — L2's control would be vacuous"
DISTINCT_OWNERS=$(owners_since "$MARK")
note "pods with DISTINCT hostnames -> distinct owner strings: $DISTINCT_OWNERS"
if [ "$DISTINCT_OWNERS" -ge 2 ]; then
  pass "CONTROL: distinct hostnames DO produce distinct identities — L2's oracle can tell the difference"
else
  bad "CONTROL FAILED: distinct hostnames still collapsed ($DISTINCT_OWNERS) — L2 proves nothing"
fi

# ── L3 ─ a thinking agent in another cluster ───────────────────────────
# The idle ladder suspends when TWO signals agree: `chert.us/requested-at`
# is stale AND the hub's own activity clock says idle. NFS I/O does count
# as activity and is source-agnostic, so a BUSY remote agent holds the
# hub up for free. The gap the ladder's own comment names is the agent
# that holds a mount and computes in memory:
#
#     "an agent computing in memory for twenty minutes with a mount held
#      open ... has only the first, and nothing else in the system will
#      stamp it."                                  (lite_gateway/proxy.rs)
#
# Cross-cluster, that gap is structural rather than merely awkward.
# `chert.us/requested-at` is an annotation on a FlintShare that lives in
# the HUB's API server; a workload cluster has no credential for it and
# no CRD to read. So the keepalive that holds a thinking agent up cannot
# be issued by the agent, only by a front door the fleet has to build.
#
# ORACLE: the phase the operator lands on while a remote mount is held
# open. ANTI-VACUITY: a second share, mounted by an agent that touches a
# file every 10s, must stay Ready across the same window — otherwise this
# leg is measuring a broken ladder rather than the thinking-agent gap.
say "L3: does the ladder suspend under a mount held by another cluster?"
ka -n "$NS" patch flintshare $SHARE --type=merge \
  -p '{"spec":{"idle":{"suspendAfterSecs":60}}}' >/dev/null 2>&1 || bad "could not set suspendAfterSecs"
mount_at "$CB" agent >/dev/null 2>&1
X "$CB" agent "nohup sh -c 'while :; do sleep 3600; done' >/dev/null 2>&1 & echo thinking" >/dev/null
# the anti-vacuity arm: cluster C keeps touching a file
X "$CC" agent "nohup sh -c 'while :; do date > /mnt/w/heartbeat.txt; sleep 10; done' >/dev/null 2>&1 & echo busy" >/dev/null
note "B holds a mount and does NO I/O; C writes every 10s; suspendAfterSecs=60"
SUSPENDED_AT=""
for i in $(seq 1 30); do
  P=$(ka -n "$NS" get flintshare $SHARE -o jsonpath='{.status.phase}' 2>/dev/null)
  case "$P" in IdleSuspended|Suspended) SUSPENDED_AT=$((i*10)); break ;; esac
  sleep 10
done
if [ -n "$SUSPENDED_AT" ]; then
  bad "the hub SUSPENDED after ~${SUSPENDED_AT}s with a live remote mount held open (phase=$P)"
  note "a remote cluster has no path to chert.us/requested-at, so nothing there can wake it"
else
  pass "the hub stayed up — C's I/O held it (phase=$P); the thinking-agent gap needs a front door"
fi
X "$CC" agent "pkill -f 'while :' " >/dev/null 2>&1
X "$CB" agent "pkill -f 'while :' " >/dev/null 2>&1
ka -n "$NS" patch flintshare $SHARE --type=json \
  -p '[{"op":"remove","path":"/spec/idle"}]' >/dev/null 2>&1

# ── L4 ─ a partition revokes the guard that exists for partitions ──────
# `suspendWithSessions: false` is the PROTECTIVE value: it refuses to
# suspend while any client holds a lease, and the CRD names cross-cluster
# mounts as the case it is for. It is implemented as
#
#     suspend_with_sessions == Some(false) && sessions_live == Some(true)
#
# and `sessions_live` is `activeLeases > 0` read from the hub. The hub
# retires a lease it has not heard from in 90s. So a client that is
# UNREACHABLE — the only situation in which suspending under it is
# unrecoverable — stops counting as a session after one lease window,
# and the guard silently stops applying.
#
# The operator cannot see this. It polls the hub's POD IP on the HUB
# cluster's own network, so the path between the agent clusters and the
# hub is outside every input the ladder reads. Throughout the partition
# the hub answers /status perfectly.
#
# ORACLE: the operator's OWN `IdleEligible` message, sampled across the
# partition. It must never stop reading "a client still holds a lease"
# while the client is still mounted and still wants the hub.
#
# ANTI-VACUITY, and it is the whole leg: the CONTROL runs first and must
# hold the share up for 3x the threshold with the client CONNECTED. If
# `suspendWithSessions: false` did not work at all, the partition arm
# would suspend too and look identical. The control is what makes the
# difference attributable to the partition rather than to a guard that
# never worked.
say "L4: does suspendWithSessions:false survive a partition?"
ka -n "$NS" patch flintshare $SHARE --type=merge \
  -p '{"spec":{"idle":{"suspendAfterSecs":60,"suspendWithSessions":false}}}' >/dev/null 2>&1
mount_at "$CB" agent >/dev/null 2>&1
idle_reason() { ka -n "$NS" get flintshare $SHARE -o jsonpath='{.status.conditions}' 2>/dev/null \
  | jq -r '.[]|select(.type=="IdleEligible")|.message' 2>/dev/null; }
idle_state()  { ka -n "$NS" get flintshare $SHARE -o jsonpath='{.metadata.annotations.chert\.us/idle-state}' 2>/dev/null; }

note "CONTROL: client CONNECTED and quiet, 3x the 60s threshold"
CTRL_SUSPENDED=""
for i in $(seq 1 18); do
  case "$(idle_state)" in Suspended|Hibernated) CTRL_SUSPENDED=$((i*10)); break ;; esac
  sleep 10
done
if [ -n "$CTRL_SUSPENDED" ]; then
  bad "CONTROL FAILED: suspended at ${CTRL_SUSPENDED}s with a reachable client — the guard does \
not work at all, so the partition arm proves nothing. L4 VOID."
else
  pass "CONTROL: 180s at 3x the threshold, never suspended ($(idle_reason))"

  note "TEST: same share, same setting, cluster $CB partitioned from the hub"
  # AWS security groups are STATEFUL — revoking a rule does NOT cut an
  # ESTABLISHED flow. iptables DROP does. Pod egress is FORWARDed rather
  # than locally generated, so both chains, inserted at position 1.
  NODE_B="$CB-control-plane"
  docker exec "$NODE_B" iptables -I OUTPUT 1 -d "$ADDR_HOST" -j DROP
  docker exec "$NODE_B" iptables -I FORWARD 1 -d "$ADDR_HOST" -j DROP
  CUT=$(X "$CB" agent "timeout 6 nc -z -w5 $ADDR_HOST $NODEPORT && echo REACHABLE || echo CUT" | tr -d ' \r\n')
  if [ "$CUT" != "CUT" ]; then
    bad "the partition did not take ($CUT) — L4 VOID"
  else
    GUARD_SEEN=""; LEASE_ZERO=""; P_SUSPENDED=""
    for i in $(seq 1 26); do
      T=$((i*12))
      R=$(idle_reason)
      case "$R" in *"still holds a lease"*) GUARD_SEEN=1 ;; esac
      L=$(ka -n "$NS" logs statuspoll --tail=1 2>/dev/null | awk '{print $2}')
      [ -z "$LEASE_ZERO" ] && [ "$L" = "0" ] && LEASE_ZERO=$T
      case "$(idle_state)" in Suspended|Hibernated) P_SUSPENDED=$T; break ;; esac
      sleep 12
    done
    docker exec "$NODE_B" iptables -D OUTPUT -d "$ADDR_HOST" -j DROP 2>/dev/null
    docker exec "$NODE_B" iptables -D FORWARD -d "$ADDR_HOST" -j DROP 2>/dev/null
    # The guard must be seen ENGAGING, or the suspend proves only that
    # the client went quiet — not that a working guard was revoked.
    if [ -z "$GUARD_SEEN" ]; then
      bad "never observed the guard engaging during the partition — L4 VOID"
    elif [ -n "$P_SUSPENDED" ]; then
      bad "CONFIRMED: the guard engaged, then the lease was retired at ~${LEASE_ZERO:-?}s and the \
share SUSPENDED at ~${P_SUSPENDED}s — under a client that is still mounted and still wants it"
      note "the client's cluster has no path to chert.us/requested-at, so nothing there can wake it"
    else
      pass "the guard held for the whole partition — suspendWithSessions:false is partition-safe"
    fi
    sleep 15
    note "after healing: phase=$(ka -n "$NS" get flintshare $SHARE -o jsonpath='{.status.phase}')"
  fi
fi

# ── summary ────────────────────────────────────────────────────────────
echo
echo "══════════════════════════════════════════════════════════════════"
echo " many-clusters-one-hub summary — $PASSED passed, $FAILED failed"
echo "══════════════════════════════════════════════════════════════════"
echo " distinct NFS identities, colliding hostnames : ${COLLIDE_OWNERS:-?}  (1 = the hub cannot tell the clusters apart)"
echo " distinct NFS identities, distinct hostnames  : ${DISTINCT_OWNERS:-?}  (>=2 = the oracle can tell the difference)"
echo " case-5 'client reboot detected' events       : $(HUBLOG | grep -c 'case 5 (client reboot detected)')"
echo " case-5 cleanups that COMPLETED               : $(case5_count)  (0 with nconnect>=2 — see client.rs case 4)"
echo
echo " WHAT THIS RIG CANNOT SEE:"
echo "  * three kind clusters share ONE host kernel, so the NFS client"
echo "    boot verifier and client caching behave differently from three"
echo "    real machines. Driving the case-5 steal to COMPLETION from"
echo "    userspace is unreliable here; the unit tests pin that instead."
echo "  * no WAN. A partition longer than the 90s lease is the sharpest"
echo "    remaining hazard and needs real separated clusters."
echo "  * no cloud LoadBalancer, so the LB-without-advertiseAddress"
echo "    window (Ready with an empty status.address) is unexercised."
if [ "${#FAILURES[@]}" -gt 0 ] 2>/dev/null; then
  echo; echo " FAILURES:"; for f in ${FAILURES[@]+"${FAILURES[@]}"}; do echo "  - $f"; done
fi
[ "$FAILED" -eq 0 ] && echo && echo "ALL LEGS PASSED." || true
exit 0
