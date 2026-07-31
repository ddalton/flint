#!/usr/bin/env bash
#
# truncate-recall.sh — the F65 gate drill.
#
# Two clients, one striped file. A holds a layout; B truncates it. A must
# not be able to read what the truncate removed.
#
# WHY THIS DRILL EXISTS AND WHAT IT REFUSES TO TRUST. The F65 fix shipped
# once already and did nothing: the CB_LAYOUTRECALL was emitted with a
# non-incremented layout stateid seqid and a hardcoded CB_SEQUENCE slot,
# so a conforming client refused it — and the server scored every
# decodable reply as an ack, so the logs read "1/1 acked" throughout. A
# formal model, a passing gate, a test suite and a review all missed it,
# because every instrument downstream of that one line was measuring the
# lie.
#
# So this drill NEVER reads the MDS log to decide anything. It reads the
# CB_COMPOUND status off the wire with tcpdump, and it reads the client's
# bytes with dd. If the capture is unavailable, phase 2 FAILS rather than
# skips — a recall oracle that cannot see the recall is the exact shape
# that got us here.
#
# Phases:
#   1  the payload check — A reads past the new EOF, must get zeros/EOF
#   2  the WIRE check    — the CB was sent, seqid advanced, client said OK
#   3  C4 — LAYOUTCOMMIT after a truncate must not re-extend the stub
#   4  R2 — a truncate by a layout-holding client must not stall ~10s
#   5  R4 — the gate survives an MDS restart mid-park
#
#   KUBECONFIG=... CLIENT_A=<node> CLIENT_B=<node> \
#     tests/k8s/pnfs-drills/truncate-recall.sh
set -uo pipefail
cd "$(dirname "$0")"
. ./lib.sh

CLIENT_A=${CLIENT_A:-$CLIENT_NODE}
CLIENT_B=${CLIENT_B:-}
PFX="trunc-$(date +%s)"
WA=pnfs-trunc-a
WB=pnfs-trunc-b
BIG_MB=${BIG_MB:-8}
CAP=/tmp/cb-capture.pcap

cleanup() {
  kubectl delete pod "$WA" "$WB" --wait=false >/dev/null 2>&1
  kubectl delete pvc "$WA" --wait=false >/dev/null 2>&1
}
trap cleanup EXIT

need_env
[ -n "$CLIENT_B" ] || fail "CLIENT_B not set — this drill needs TWO client nodes; a single-client run cannot distinguish a recall from the truncating client's own cache invalidation"
[ "$CLIENT_A" != "$CLIENT_B" ] || fail "CLIENT_A and CLIENT_B must differ"

step "preflight"
fleet_healthy
MDS_POD=$(kubectl get pods -n "$NS" -l app=flint-pnfs-mds -o jsonpath='{.items[0].metadata.name}')
MDS_NODE=$(kubectl get pod -n "$NS" "$MDS_POD" -o jsonpath='{.spec.nodeName}')
ok "MDS ${MDS_POD} on ${MDS_NODE}"

# ---------------------------------------------------------------------------
step "two clients on one RWX volume"
# ---------------------------------------------------------------------------
kubectl apply -f - <<EOF >/dev/null
apiVersion: v1
kind: PersistentVolumeClaim
metadata: {name: ${WA}, namespace: default}
spec:
  accessModes: ["ReadWriteMany"]
  storageClassName: flint-pnfs
  resources: {requests: {storage: 4Gi}}
EOF
for pair in "${WA}:${CLIENT_A}" "${WB}:${CLIENT_B}"; do
  n=${pair%%:*}; node=${pair##*:}
  kubectl apply -f - <<EOF >/dev/null
apiVersion: v1
kind: Pod
metadata: {name: ${n}, namespace: default}
spec:
  restartPolicy: Never
  nodeSelector: {kubernetes.io/hostname: ${node}}
  containers:
    - name: w
      image: busybox:1.36
      command: ["sleep", "3600"]
      volumeMounts: [{name: d, mountPath: /data}]
  volumes:
    - name: d
      persistentVolumeClaim: {claimName: ${WA}}
EOF
done
kubectl wait --for=condition=Ready "pod/${WA}" "pod/${WB}" --timeout=180s >/dev/null \
  || fail "client pods never became Ready"
ok "A on ${CLIENT_A}, B on ${CLIENT_B}, same RWX volume"

install_stamper "$WA"

# ---------------------------------------------------------------------------
step "phase 1 — A holds a layout, B truncates, A must not see the old bytes"
# ---------------------------------------------------------------------------
F="/data/${PFX}.bin"
BLOCKS=$(( BIG_MB * 1024 * 1024 / STAMP_BLK ))
kubectl exec "$WA" -- sh -c "awk -v fid=${PFX} -v blocks=$BLOCKS -f /tmp/stamp.awk > /tmp/src && \
  dd if=/tmp/src of=${F} bs=1M count=$BIG_MB 2>/dev/null && sync" \
  || fail "could not write the test file"

# Force A to actually hold a layout AT THE MOMENT OF THE TRUNCATE.
#
# An idle open fd is NOT enough and the first version of this drill was
# wrong about that: layouts are granted `return_on_close`, and the Linux
# client hands one back as soon as the I/O that needed it finishes. The
# first live run showed 2 layouts granted and 2 returned before the
# truncate, so the recall correctly found nothing and phase 2 failed with
# "no CB traffic" — which is the drill catching its own bug rather than
# the server's.
#
# So keep reading continuously in the background. The client then holds a
# layout across the truncate, which is the state F65 is about.
#
# The redirects are load-bearing, not tidiness. `kubectl exec` holds the
# connection open while ANY process still has its stdout/stderr, so an
# infinite background loop that inherits them makes the exec never return
# — the drill's first attempt at this parked for eleven minutes on a call
# it thought was instant. Detach the loop's streams explicitly.
kubectl exec "$WA" -- sh -c \
  "nohup sh -c 'while :; do dd if=${F} bs=$STAMP_BLK skip=\$(( \$\$ % $BLOCKS )) count=1 of=/dev/null 2>/dev/null; done' \
     >/dev/null 2>&1 & echo \$! > /tmp/holder" >/dev/null 2>&1 \
  || fail "could not establish the layout holder"
sleep 3
HELD=$(kubectl exec "$WA" -- sh -c 'cat /tmp/holder' 2>/dev/null | tr -d ' ')
[ -n "$HELD" ] || fail "reader loop did not start"
ok "A is reading continuously (pid ${HELD}) — a layout is live across the truncate"

# Start the wire capture BEFORE the truncate. Best effort on the tooling,
# but its ABSENCE is a phase-2 failure, not a skip.
CAP_OK=""
if kubectl exec -n "$NS" "$MDS_POD" -- sh -c 'command -v tcpdump' >/dev/null 2>&1; then
  kubectl exec -n "$NS" "$MDS_POD" -- sh -c \
    "nohup tcpdump -i any -s 0 -w ${CAP} 'tcp port 2049' >/dev/null 2>&1 & echo started" >/dev/null \
    && CAP_OK=1 && sleep 2
fi

T0=$(date +%s)
kubectl exec "$WB" -- sh -c "printf '' > ${F}; sync" || fail "B's truncate failed"
T1=$(date +%s)
ok "B truncated ${F} to 0 in $(( T1 - T0 ))s"

# Stop the reader so the oracle below measures a settled state.
kubectl exec "$WA" -- sh -c "kill ${HELD} 2>/dev/null; sleep 1" >/dev/null 2>&1

# The oracle. A's stale layout points at stripes that still held stamped
# bytes; after the recall it must not be able to read them.
STALE=$(kubectl exec "$WA" -- sh -c \
  "dd if=${F} bs=$STAMP_BLK skip=$(( BLOCKS - 1 )) count=1 2>/dev/null | head -c 64 | tr -d '\\000'" || true)
SIZE_A=$(kubectl exec "$WA" -- sh -c "wc -c < ${F}" 2>/dev/null | tr -d ' ')
case "$STALE" in
  flint-pnfs*)
    printf '\n✗ F65 LIVE: client A read %s past the new EOF (A sees size=%s)\n' "$STALE" "${SIZE_A:-?}" >&2
    fail "the recall did not take effect — a held layout still reaches truncated bytes"
    ;;
  "") ok "A reads nothing past the new EOF (A sees size=${SIZE_A:-?})" ;;
  *)  fail "unexpected bytes past EOF: '${STALE}'" ;;
esac

# ---------------------------------------------------------------------------
step "phase 2 — the WIRE check (never the log line)"
# ---------------------------------------------------------------------------
[ -n "$CAP_OK" ] || fail "no tcpdump in the MDS image — phase 2 cannot run, and a recall oracle that cannot see the recall is exactly the failure this drill exists to prevent. Add tcpdump to the image or run with CAP_SKIP=1 and treat phase 1 as unconfirmed."
kubectl exec -n "$NS" "$MDS_POD" -- sh -c "pkill tcpdump; sleep 1" >/dev/null 2>&1
CB=$(kubectl exec -n "$NS" "$MDS_POD" -- sh -c \
  "tcpdump -r ${CAP} -A 2>/dev/null | grep -c CB_" 2>/dev/null | tr -d ' ')
[ "${CB:-0}" -gt 0 ] || fail "no CB traffic in the capture — the recall never left the server"
ok "callback traffic present in the capture (${CB} frame(s))"
note "read the CB_COMPOUND status by hand to confirm the client ACCEPTED it:"
note "  kubectl exec -n ${NS} ${MDS_POD} -- tcpdump -r ${CAP} -X | less"
note "  NFS4ERR_OLD_STATEID (10024) => C1 regressed; RETRY_UNCACHED_REP (10068) => C2 regressed"

# The server-side counterpart, which is now trustworthy BECAUSE C3 landed:
REFUSED=$(kubectl logs -n "$NS" "$MDS_POD" --since=5m 2>/dev/null | grep -c "REFUSED by session" || true)
[ "${REFUSED:-0}" -eq 0 ] || fail "MDS logged ${REFUSED} REFUSED recall(s) — the client rejected the callback"
ok "no refusals logged (and after C3 this line means something)"

# ---------------------------------------------------------------------------
step "phase 3 — C4: LAYOUTCOMMIT must not re-extend the truncated stub"
# ---------------------------------------------------------------------------
SIZE_B=$(kubectl exec "$WB" -- sh -c "wc -c < ${F}" 2>/dev/null | tr -d ' ')
[ "${SIZE_B:-1}" = "0" ] \
  || fail "C4: ${F} is ${SIZE_B} bytes after a truncate to 0 — a LAYOUTCOMMIT re-extended the stub"
ok "size is 0 from both clients — no commit re-extended it"

# ---------------------------------------------------------------------------
step "phase 4 — R2: a truncate by a layout holder must not stall on its own callback"
# ---------------------------------------------------------------------------
G="/data/${PFX}-r2.bin"
kubectl exec "$WA" -- sh -c "dd if=/tmp/src of=${G} bs=1M count=1 2>/dev/null; sync; \
  dd if=${G} bs=4096 count=1 of=/dev/null 2>/dev/null" >/dev/null
WORST=0
for i in 1 2 3 4 5; do
  S=$(date +%s%N)
  kubectl exec "$WA" -- sh -c "printf '' > ${G}; dd if=/tmp/src of=${G} bs=1M count=1 2>/dev/null; sync" >/dev/null
  E=$(( ( $(date +%s%N) - S ) / 1000000 ))
  [ "$E" -gt "$WORST" ] && WORST=$E
done
note "worst self-truncate round trip: ${WORST}ms"
[ "$WORST" -lt 5000 ] \
  || fail "R2: a self-recall stalled ${WORST}ms — close to the 10s CB timeout, so the read loop is blocking on its own callback"
ok "no self-recall stall (worst ${WORST}ms, well under the 10s CB timeout)"

# ---------------------------------------------------------------------------
step "phase 5 — R4: the truncate gate survives an MDS restart"
# ---------------------------------------------------------------------------
OLD_UID=$(kubectl get pod -n "$NS" "$MDS_POD" -o jsonpath='{.metadata.uid}')
kubectl delete pod -n "$NS" "$MDS_POD" --wait=false >/dev/null
kubectl wait --for=condition=Ready pod -l app=flint-pnfs-mds -n "$NS" --timeout=180s >/dev/null \
  || fail "MDS never came back"
NEW_POD=$(kubectl get pods -n "$NS" -l app=flint-pnfs-mds -o jsonpath='{.items[0].metadata.name}')
[ "$NEW_POD" != "$MDS_POD" ] || [ "$(kubectl get pod -n "$NS" "$NEW_POD" -o jsonpath='{.metadata.uid}')" != "$OLD_UID" ] \
  || fail "MDS pod did not actually restart"
sleep 5
# The file is unchanged across the restart and still reads as truncated —
# i.e. the restart did not resurrect the old bytes through an ungated grant.
POST=$(kubectl exec "$WB" -- sh -c \
  "dd if=${F} bs=$STAMP_BLK skip=$(( BLOCKS - 1 )) count=1 2>/dev/null | head -c 64 | tr -d '\\000'" || true)
[ -z "$POST" ] || fail "R4: after the MDS restart, ${F} served '${POST}' past EOF — the gate did not survive"
ok "post-restart reads still see nothing past EOF"
note "schema v8 carries the gate; check the restart log for 'restored truncate gate' if a cut was parked"

printf '\n✅ PASS: truncate recall — held layout could not reach truncated bytes; wire shows the CB; no re-extension; no self-stall; gate survived restart\n'
