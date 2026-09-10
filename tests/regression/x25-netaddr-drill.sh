#!/usr/bin/env bash
# X25 — the host:port joiner, on a live cluster.
#
#   ./tests/regression/x25-netaddr-drill.sh <fixed-image> <control-image>
#
# Two claims, drilled as A/B against images built from the SAME tree
# except for the fix.
#
# CLAIM 1 — REFUTED BY THIS DRILL, 2026-09-10, and kept here because the
# refutation is the point. The claim was that `bind.address: "::"` could
# not start, because `format!("{}:{}")` yields `:::2049` and no
# socket-address parser accepts it. The second half is true and the
# conclusion does not follow: `TcpListener::bind` takes `&str` through
# `ToSocketAddrs`, which on a strict-parse failure falls back to
# splitting at the LAST colon and calling getaddrinfo — and getaddrinfo
# resolves "::" happily. Both arms bind the identical socket
# (00000000000000000000000000000000:0801). Legs L1-L4 pin that, so the
# bind-site edits are cosmetic and nobody re-derives the claim later.
#
# CLAIM 1b — what is ACTUALLY broken, found by asking why L2 passed.
# The sites that parse STRICTLY rather than binding a string are the
# real ones: `mds/server.rs` gRPC control server does
# `.parse::<SocketAddr>().expect(...)` on the same `format!`. With
# `::` it panics — inside a SPAWNED TOKIO TASK, so it kills the task and
# not the process. The MDS keeps running, keeps serving NFS, logs "gRPC
# control server started on port 50051", reports Running with zero
# restarts to Kubernetes, and has NO control plane at all: no DS can
# ever register. A liveness probe cannot see it; only the absence of a
# listening socket can. That is leg L7.
#
# CLAIM 2 — CONFIRMED AT THE SERVER, not at the client. A DS endpoint
# that cannot become an RFC 5665 universal
# address must FAIL the GETDEVICEINFO, not be written to the wire as
# itself. The old fallback encoded `host:port` where a uaddr belongs:
# a well-formed netaddr4 carrying nonsense, so the client takes the
# device, caches it, and the fault surfaces later as I/O that goes
# nowhere. What the drill showed: the client outcome is EIO in BOTH
# arms, so the fix does not change what the client sees. What it changes
# is whether anyone can find out WHY — the fixed arm names the endpoint
# and the reason, the control arm logs nothing at all. Legs L5/L6 score
# on the SERVER's log differential for that reason: scoring them on the
# client outcome made L6 "pass" while both arms behaved identically,
# which is a leg that cannot fail.
#
# WHY THE MATRIX IS 2x2 AND NOT 2x1. The interesting leg (control image,
# `::`) is a pod that never becomes Ready — and a pod can fail to become
# Ready for a hundred reasons that have nothing to do with this fix. So
# every arm runs BOTH bind addresses. The control image on `0.0.0.0`
# must come up and serve; if it does not, the control image is simply
# broken and leg 2 proves nothing. A drill whose control fails for the
# wrong reason is worse than no drill: it reports GREEN.
set -uo pipefail

FIXED_IMG=${1:?usage: x25-netaddr-drill.sh <fixed-image> <control-image>}
CTRL_IMG=${2:?usage: x25-netaddr-drill.sh <fixed-image> <control-image>}
NS=${NS:-x25}
PORT=2049
# 2049 = 0x0801. /proc/net/tcp6 is in EVERY container without installing
# a package, which `ss` is not — the listener's ADDRESS FAMILY is the
# whole point of claim 1, so the oracle must not depend on a package
# that may be absent in the runtime image.
PORT_HEX=0801

pass=0; fail=0; skip=0
ok()   { echo "  ✅ $*"; pass=$((pass+1)); }
no()   { echo "  ❌ $*"; fail=$((fail+1)); }
inc()  { echo "  ⚠️  INCONCLUSIVE: $*"; skip=$((skip+1)); }
hdr()  { echo; echo "═══ $* ═══"; }

kubectl create namespace "$NS" >/dev/null 2>&1 || true

# ── deploy one hub arm ───────────────────────────────────────────────
# $1 name  $2 image  $3 bind address  $4 mode  $5 dataServers yaml block
deploy() {
    local name=$1 img=$2 bind=$3 mode=$4 ds=$5
    kubectl -n "$NS" delete deploy,svc,cm "$name" --ignore-not-found >/dev/null 2>&1
    kubectl -n "$NS" apply -f - >/dev/null <<EOF
apiVersion: v1
kind: ConfigMap
metadata: { name: $name }
data:
  mds.yaml: |
    apiVersion: chert.us/v1alpha1
    kind: PnfsConfig
    mode: $mode
    mds:
      bind:
        address: "$bind"
        port: $PORT
      layout:
        type: file
        stripeSize: 8388608
        policy: stripe
      dataServers: $ds
      state:
        backend: sqlite
        config:
          path: /data/state/state.db
    exports:
      - path: /data/exports
        fsid: 1
        options: [rw, sync, no_subtree_check]
        access:
          - network: 0.0.0.0/0
            permissions: rw
    logging:
      level: info
      format: text
---
apiVersion: apps/v1
kind: Deployment
metadata: { name: $name }
spec:
  replicas: 1
  selector: { matchLabels: { app: $name } }
  template:
    metadata: { labels: { app: $name } }
    spec:
      containers:
        - name: mds
          image: $img
          imagePullPolicy: IfNotPresent
          command: ["/usr/local/bin/flint-pnfs-mds"]
          args: ["--config", "/etc/flint/mds.yaml"]
          ports: [{ containerPort: $PORT }]
          readinessProbe:
            tcpSocket: { port: $PORT }
            initialDelaySeconds: 3
            periodSeconds: 3
          volumeMounts:
            - { name: cfg,  mountPath: /etc/flint }
            - { name: data, mountPath: /data }
      volumes:
        - { name: cfg,  configMap: { name: $name } }
        - { name: data, emptyDir: {} }
---
apiVersion: v1
kind: Service
metadata: { name: $name }
spec:
  selector: { app: $name }
  ports: [{ port: $PORT, targetPort: $PORT }]
EOF
}

ready() { kubectl -n "$NS" wait --for=condition=Available "deploy/$1" --timeout="${2:-120s}" >/dev/null 2>&1; }
podof() { kubectl -n "$NS" get pod -l "app=$1" -o jsonpath='{.items[0].metadata.name}' 2>/dev/null; }

# Which family is the listener on? Reads the kernel's own tables.
listener_family() {
    local pod=$1 v6 v4
    v6=$(kubectl -n "$NS" exec "$pod" -- sh -c "grep -ci ':$PORT_HEX ' /proc/net/tcp6 2>/dev/null || echo 0" 2>/dev/null | tr -d '\r')
    v4=$(kubectl -n "$NS" exec "$pod" -- sh -c "grep -ci ':$PORT_HEX ' /proc/net/tcp  2>/dev/null || echo 0" 2>/dev/null | tr -d '\r')
    echo "${v6:-0} ${v4:-0}"
}

# ── the client: an IPv4 kernel mount + a data round trip ─────────────
client_up() {
    kubectl -n "$NS" delete pod x25-client --ignore-not-found >/dev/null 2>&1
    kubectl -n "$NS" apply -f - >/dev/null <<'EOF'
apiVersion: v1
kind: Pod
metadata: { name: x25-client }
spec:
  containers:
    - name: c
      image: ubuntu:24.04
      command: ["sh","-c","apt-get update -qq && apt-get install -y -qq nfs-common >/dev/null 2>&1; sleep 100000"]
      securityContext: { privileged: true }
EOF
    kubectl -n "$NS" wait --for=condition=Ready pod/x25-client --timeout=180s >/dev/null 2>&1 || return 1
    # mount.nfs4 has to actually exist before any leg trusts a mount failure.
    for _ in $(seq 1 30); do
        kubectl -n "$NS" exec x25-client -- sh -c 'command -v mount.nfs4 >/dev/null' 2>/dev/null && return 0
        sleep 5
    done
    return 1
}

# Mount $1 (service name), write 64MB, read it back, compare digests.
roundtrip() {
    local svc=$1
    kubectl -n "$NS" exec x25-client -- sh -c "
        set -e
        umount -f /mnt/x25 2>/dev/null || true
        mkdir -p /mnt/x25
        mount -t nfs4 -o vers=4.1,nolock,soft,timeo=50,retrans=2 $svc.$NS.svc.cluster.local:/ /mnt/x25
        mountpoint -q /mnt/x25
        dd if=/dev/urandom of=/tmp/src bs=1M count=64 status=none
        cp /tmp/src /mnt/x25/payload
        sync
        a=\$(sha256sum /tmp/src        | cut -d' ' -f1)
        b=\$(sha256sum /mnt/x25/payload | cut -d' ' -f1)
        umount -f /mnt/x25 || true
        [ \"\$a\" = \"\$b\" ] || { echo DIGEST-MISMATCH; exit 3; }
        echo ROUNDTRIP-OK
    " 2>&1 | tr -d '\r'
}

echo "════════════════════════════════════════════════════════════════"
echo "X25 netaddr drill"
echo "  fixed:   $FIXED_IMG"
echo "  control: $CTRL_IMG"
echo "  started: $(date -u +%FT%TZ)"
echo "════════════════════════════════════════════════════════════════"

# The image digests, not the tags. This repo has shipped images whose
# tag said one version and whose binaries were another; two arms that
# differ only by tag are two arms that might be the same binary.
hdr "L0 — the two images must be DIFFERENT artifacts"
deploy x25-probe-f "$FIXED_IMG" "0.0.0.0" standalone "[]"
deploy x25-probe-c "$CTRL_IMG"  "0.0.0.0" standalone "[]"
ready x25-probe-f 180s; ready x25-probe-c 180s
DF=$(kubectl -n "$NS" get pod -l app=x25-probe-f -o jsonpath='{.items[0].status.containerStatuses[0].imageID}' 2>/dev/null)
DC=$(kubectl -n "$NS" get pod -l app=x25-probe-c -o jsonpath='{.items[0].status.containerStatuses[0].imageID}' 2>/dev/null)
echo "  fixed   imageID: $DF"
echo "  control imageID: $DC"
if [ -z "$DF" ] || [ -z "$DC" ]; then inc "could not read both imageIDs"
elif [ "$DF" = "$DC" ]; then no "L0 the two arms resolved to the SAME image — every later leg is vacuous"
else ok "L0 the arms are distinct artifacts by digest"; fi

if ! client_up; then
    echo "  ⚠️  the client pod never got mount.nfs4 — mount legs cannot run"
    CLIENT_OK=0
else
    CLIENT_OK=1
fi

# ── the 2x2 ──────────────────────────────────────────────────────────
# arm  bind        expect
run_bind_leg() {
    local leg=$1 img=$2 bind=$3 expect=$4 name=$5
    hdr "$leg — $(basename "$img" | cut -c1-40) with bind.address=\"$bind\" (expect: $expect)"
    deploy "$name" "$img" "$bind" standalone "[]"
    if ready "$name" 120s; then
        local pod; pod=$(podof "$name")
        read -r v6 v4 <<< "$(listener_family "$pod")"
        echo "  listeners on port $PORT: tcp6=$v6 tcp4=$v4"
        if [ "$expect" = "serves" ]; then
            ok "$leg the hub came up"
            if [ "$bind" = "::" ]; then
                if [ "${v6:-0}" -ge 1 ]; then ok "$leg the listener is on tcp6 — one socket, both families"
                else no "$leg asked for :: but no tcp6 listener — it silently fell back"; fi
            fi
            if [ "$CLIENT_OK" = 1 ]; then
                local r; r=$(roundtrip "$name")
                if echo "$r" | grep -q ROUNDTRIP-OK; then ok "$leg an IPv4 kernel client mounted and 64MB round-tripped"
                else no "$leg the round trip failed: $(echo "$r" | tail -2 | tr '\n' ' ')"; fi
            else inc "$leg no client — mount not attempted"; fi
        else
            no "$leg it came up, and the claim says it must NOT"
        fi
    else
        if [ "$expect" = "refuses" ]; then
            local pod log; pod=$(podof "$name")
            log=$(kubectl -n "$NS" logs "$pod" --tail=40 2>/dev/null; kubectl -n "$NS" logs "$pod" --previous --tail=40 2>/dev/null)
            echo "$log" | tail -6 | sed 's/^/      | /'
            # It must fail FOR THE STATED REASON. A pod that crashloops
            # for an unrelated reason would otherwise read as a pass.
            if echo "$log" | grep -qiE 'invalid socket address|invalid.*address|:::|failed to parse|AddrNotAvail|address'; then
                ok "$leg refused to start, and the log names the address"
            else
                inc "$leg never became ready, but the log does not name the address — cause unproven"
            fi
        else
            no "$leg it did NOT come up, and the claim says it must"
            kubectl -n "$NS" logs "$(podof "$name")" --tail=20 2>/dev/null | sed 's/^/      | /'
        fi
    fi
}

run_bind_leg L1 "$FIXED_IMG" "::"      serves  x25-fixed-v6
run_bind_leg L2 "$CTRL_IMG"  "::"      serves  x25-ctrl-v6   # refuted: it binds fine
run_bind_leg L3 "$FIXED_IMG" "0.0.0.0" serves  x25-fixed-v4
run_bind_leg L4 "$CTRL_IMG"  "0.0.0.0" serves  x25-ctrl-v4

# ── GETDEVICEINFO: an unencodable DS endpoint ────────────────────────
# `.invalid` is reserved by RFC 2606 and can never resolve, so the
# endpoint can never become the IPv4 octets a files-layout universal
# address requires.
BAD_DS='[{"deviceId":"ds-phantom","endpoint":"no-such-ds.invalid:2049","bdevs":["b0"]}]'

run_gdi_leg() {
    local leg=$1 img=$2 expect=$3 name=$4
    hdr "$leg — GETDEVICEINFO with an unresolvable DS (expect server log: $expect)"
    deploy "$name" "$img" "0.0.0.0" mds "$BAD_DS"
    if ! ready "$name" 120s; then
        inc "$leg the MDS did not come up in mds mode — leg not run"
        return
    fi
    if [ "$CLIENT_OK" != 1 ]; then inc "$leg no client to provoke a LAYOUTGET"; return; fi
    # Provoke the layout; the client outcome is NOT the oracle (it is EIO
    # in both arms) — the server's own account of why is.
    roundtrip "$name" >/dev/null 2>&1
    local pod lg named
    pod=$(podof "$name")
    lg=$(kubectl -n "$NS" logs "$pod" 2>/dev/null | grep -c 'LAYOUTGET granted')
    named=$(kubectl -n "$NS" logs "$pod" 2>/dev/null | grep -c 'GETDEVICEINFO: device address will not encode')
    echo "  LAYOUTGET granted: $lg    GETDEVICEINFO refusals named: $named"
    if [ "$lg" -lt 1 ]; then
        inc "$leg no LAYOUTGET was ever granted — the encode path was never reached"
        return
    fi
    if [ "$expect" = names-it ]; then
        if [ "$named" -ge 1 ]; then
            kubectl -n "$NS" logs "$pod" 2>/dev/null | grep 'will not encode' | tail -1 | cut -c1-200 | sed 's/^/      | /'
            ok "$leg the refusal is raised and names the endpoint"
        else
            no "$leg the encode failure was not reported — the error is still being swallowed"
        fi
    else
        if [ "$named" -eq 0 ]; then
            ok "$leg the control says NOTHING about it — the silence the fix removes"
        else
            no "$leg the control named it, so the two arms are not different here"
        fi
    fi
}

run_gdi_leg L5 "$FIXED_IMG" names-it x25-fixed-gdi
run_gdi_leg L6 "$CTRL_IMG"  silent   x25-ctrl-gdi

# ── L7 — the leg the refutation of claim 1 uncovered ─────────────────
# `mode: mds` starts the gRPC control server, whose bind address is
# `.parse::<SocketAddr>()`d STRICTLY instead of being handed to
# TcpListener::bind as a string. That is the difference: bind() falls
# back to getaddrinfo, parse() does not.
#
# The oracle is the LISTENING SOCKET, not the pod status and not the
# log. The pod is Running with zero restarts in BOTH arms and BOTH log
# "gRPC control server started on port 50051" — the control arm logs
# that and then panics in the spawned task. A drill that believed either
# the phase or the log would score this GREEN.
#   0801 = 2049 (NFS)   C383 = 50051 (gRPC control)
run_grpc_leg() {
    local leg=$1 img=$2 expect=$3 name=$4
    hdr "$leg — mode:mds, bind \"::\" — does the gRPC control plane SURVIVE? (expect: $expect)"
    deploy "$name" "$img" "::" mds "[]"
    if ! ready "$name" 150s; then inc "$leg the MDS never came up"; return; fi
    local pod ports grpc phase restarts
    pod=$(podof "$name")
    phase=$(kubectl -n "$NS" get pod "$pod" -o jsonpath='{.status.phase}' 2>/dev/null)
    restarts=$(kubectl -n "$NS" get pod "$pod" -o jsonpath='{.status.containerStatuses[0].restartCount}' 2>/dev/null)
    ports=$(kubectl -n "$NS" exec "$pod" -- sh -c 'cat /proc/net/tcp /proc/net/tcp6 2>/dev/null | awk "NR>1 && \$4==\"0A\" {print \$2}" | sort -u' 2>/dev/null | tr -d '\r')
    grpc=$(echo "$ports" | grep -ci ':C383' || true)
    echo "  pod phase=$phase restarts=$restarts   (Kubernetes is content either way)"
    echo "  claims in the log: $(kubectl -n "$NS" logs "$pod" 2>/dev/null | grep -c 'gRPC control server started')"
    echo "  panics:            $(kubectl -n "$NS" logs "$pod" 2>/dev/null | grep -ci 'Invalid gRPC address')"
    echo "  listening sockets: $(echo "$ports" | tr '\n' ' ')"
    if [ "$expect" = survives ]; then
        if [ "${grpc:-0}" -ge 1 ]; then ok "$leg the control plane is listening on 50051"
        else no "$leg NO gRPC listener — the control plane died and the pod still reports healthy"; fi
    else
        if [ "${grpc:-0}" -eq 0 ]; then
            ok "$leg the control plane is GONE while the pod reports Running/$restarts restarts — the defect"
        else no "$leg the control arm kept its gRPC listener, so this leg shows nothing"; fi
    fi
}

run_grpc_leg L7a "$FIXED_IMG" survives x25-l7-fixed
run_grpc_leg L7b "$CTRL_IMG"  dies     x25-l7-ctrl

hdr "RESULT"
echo "  pass=$pass  fail=$fail  inconclusive=$skip"
echo "  finished: $(date -u +%FT%TZ)"
echo
echo "  NOT COVERED BY THIS DRILL (the tooling has no dual-stack knob):"
echo "    - lite_operator::reconcile::address_of bracketing an IPv6 LoadBalancer ingress"
echo "    - the pod_ip URL sites (driver.rs, controller_operator.rs, dashboard)"
echo "    Both need IPv6 addresses to exist in the cluster. Unit tests only."
[ "$fail" -eq 0 ] || exit 1
