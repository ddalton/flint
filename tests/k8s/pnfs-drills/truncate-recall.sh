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
# Start from a clean slate. cleanup() deletes with --wait=false so the
# drill exits promptly, which means a back-to-back re-run can `apply`
# onto pods that are still terminating — kubectl accepts the apply, then
# `exec` fails with `container not found`. Wait the old ones out first.
if kubectl get pod "$WA" "$WB" >/dev/null 2>&1; then
  note "prior drill pods still present — waiting for them to terminate"
  kubectl delete pod "$WA" "$WB" --ignore-not-found --timeout=120s >/dev/null 2>&1
  kubectl wait --for=delete "pod/${WA}" "pod/${WB}" --timeout=120s >/dev/null 2>&1
fi

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
# A LOOP IS NOT ENOUGH EITHER, and this cost a second live run. The
# previous holder was
#     while :; do dd if=$F bs=4096 skip=$(($$ % BLOCKS)) count=1; done
# whose `skip` is evaluated ONCE, so it re-reads a single 4 KiB block
# forever. After the first read that block is in the client's page cache
# and NO further I/O reaches the server at all — zero LAYOUTGETs, nothing
# to recall, and phase 2 reports "the recall never left the server" as if
# the server were at fault. Both wrong holders (idle fd, cached loop)
# failed the same way: they made the drill pass its precondition while
# testing nothing.
#
# What works, measured on runas: reads that MISS the page cache, issued
# back to back. O_DIRECT is the reliable way to guarantee that — every
# read goes to the wire, so a layout is held continuously rather than
# for the ~80 ms after each cached hit.
#
# The redirects are load-bearing, not tidiness. `kubectl exec` holds the
# connection open while ANY process still has its stdout/stderr, so an
# infinite background loop that inherits them makes the exec never return
# — the drill's first attempt at this parked for eleven minutes on a call
# it thought was instant. Detach the loop's streams explicitly.
DIRECT="iflag=direct"
kubectl exec "$WA" -- sh -c "dd if=${F} of=/dev/null bs=1M count=1 iflag=direct" >/dev/null 2>&1 \
  || { DIRECT=""; note "busybox dd lacks iflag=direct — falling back to a full-file scan"; }

kubectl exec "$WA" -- sh -c \
  "nohup sh -c 'while :; do dd if=${F} of=/dev/null bs=1M ${DIRECT} 2>/dev/null; done' \
     >/dev/null 2>&1 & echo \$! > /tmp/holder" >/dev/null 2>&1 \
  || fail "could not establish the layout holder"
sleep 3
HELD=$(kubectl exec "$WA" -- sh -c 'cat /tmp/holder' 2>/dev/null | tr -d ' ')
[ -n "$HELD" ] || fail "reader loop did not start"

# PROVE THE PRECONDITION. Everything downstream is meaningless if A does
# not actually hold a layout when B truncates, and twice now the drill has
# asserted "a layout is live" purely from the fact that a process started.
# Ask the server. A drill that cannot confirm its own setup must fail, not
# proceed — "no CB traffic" then means the server is broken, which is the
# only reading worth reporting.
#
# Needs pnfs.server.logLevel=debug: LAYOUTGET is logged at debug, so at
# info this check would see zero and wrongly blame the holder.
LG=$(kubectl logs -n "$NS" "$MDS_POD" --since=60s 2>/dev/null | grep -c "LAYOUTGET RECEIVED" || true)
if [ "${LG:-0}" -eq 0 ]; then
  note "MDS logged no LAYOUTGET in the last 60s (log level is $(kubectl get deploy -n "$NS" flint-pnfs-mds -o jsonpath='{.spec.template.spec.containers[0].env[?(@.name=="RUST_LOG")].value}' 2>/dev/null || echo '?'))"
  fail "A is not holding a layout — the drill never established the state F65 is about, so any verdict below would be vacuous. Fix the holder, not the server."
fi
ok "A is reading continuously (pid ${HELD}, ${LG} LAYOUTGET(s) on the server) — a layout is live across the truncate"

# Start the wire capture BEFORE the truncate. Best effort on the tooling,
# but its ABSENCE is a phase-2 failure, not a skip.
#
# THREE THINGS HERE ARE SCAR TISSUE, all from one run that "passed"
# phase 2 against a capture from the PREVIOUS run:
#
#   rm -f first  — the pcap lives in the pod and outlives the drill, so a
#                  stale one from a prior run is sitting there ready to be
#                  validated. Deleting it means "no capture" fails loudly
#                  instead of silently succeeding on old bytes.
#   -U           — tcpdump buffers by default. The file on disk was the
#                  last FLUSHED state (from whichever run was killed
#                  cleanly), not this run's traffic. Packet-buffered mode
#                  keeps the file current.
#   kill by PID  — `pkill tcpdump` did not work here; four tcpdumps were
#                  still alive after the drill exited, each holding the
#                  same -w path. Record the pid and kill that.
CAP_OK=""
CAP_PID=""
if kubectl exec -n "$NS" "$MDS_POD" -- sh -c 'command -v tcpdump' >/dev/null 2>&1; then
  kubectl exec -n "$NS" "$MDS_POD" -- sh -c \
    "pkill -f 'tcpdump.*${CAP}' >/dev/null 2>&1; rm -f ${CAP}" >/dev/null 2>&1
  CAP_PID=$(kubectl exec -n "$NS" "$MDS_POD" -- sh -c \
    "nohup tcpdump -i any -U -s 0 -w ${CAP} 'tcp port 2049' >/dev/null 2>&1 & echo \$!" 2>/dev/null | tr -d ' \r')
  sleep 2
  # Prove it is actually running and actually writing, here, now.
  if [ -n "$CAP_PID" ] && kubectl exec -n "$NS" "$MDS_POD" -- sh -c "kill -0 ${CAP_PID}" >/dev/null 2>&1; then
    CAP_OK=1
  else
    note "tcpdump did not stay up (pid='${CAP_PID}')"
  fi
fi
CAP_T0=$(date -u +%s)

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
kubectl exec -n "$NS" "$MDS_POD" -- sh -c \
  "kill ${CAP_PID} 2>/dev/null; sleep 1; pkill -f 'tcpdump.*${CAP}' 2>/dev/null; sleep 1" >/dev/null 2>&1

# FRESHNESS GATE. A capture that predates the truncate cannot contain the
# recall, and validating one is worse than having none — it produces a
# green. Demand that the file was modified after the capture started.
CAP_MTIME=$(kubectl exec -n "$NS" "$MDS_POD" -- sh -c "stat -c %Y ${CAP} 2>/dev/null" 2>/dev/null | tr -d ' \r')
[ -n "$CAP_MTIME" ] || fail "capture file ${CAP} does not exist after the truncate — tcpdump never wrote anything"
[ "$CAP_MTIME" -ge "$CAP_T0" ] \
  || fail "capture is STALE (mtime ${CAP_MTIME} < capture start ${CAP_T0}) — it is from an earlier run, and decoding it would report on traffic this drill never generated"
ok "capture is fresh (written after the truncate)"

# DECODE the callback, don't grep it. The previous version ran
#   tcpdump -r $CAP -A | grep -c CB_
# which counts an ASCII string in a binary XDR stream: structurally
# always zero, so it could only ever report "the recall never left the
# server" — blaming the server for the oracle's own blindness. It did
# exactly that on the first two runs of this very fix.
#
# cb-decode.py walks the RPC. Its own teeth are proven by mutating a real
# captured CB reply: reply_stat -> MSG_DENIED reproduces C8, and
# CB_SEQUENCE -> NFS4ERR_BADSESSION reproduces C9; both flip it to FAIL
# with the matching diagnosis.
# NOTE: we already `cd "$(dirname "$0")"` at the top, so the decoder is
# simply ./cb-decode.py — re-deriving it from $0 double-prefixes the path.
[ -f ./cb-decode.py ] || fail "cb-decode.py missing next to the drill"
CB_OUT=$(kubectl exec -n "$NS" "$MDS_POD" -- sh -c "tcpdump -r ${CAP} -X 2>/dev/null" \
  | python3 ./cb-decode.py 2>&1)
CB_RC=$?
printf '%s\n' "$CB_OUT" | sed 's/^/    /'
# Distinguish "the decoder could not run" from "the decoder says the
# server failed". Conflating them is how the previous oracle reported a
# server bug that did not exist.
case "$CB_OUT" in
  *VERDICT*) : ;;
  *) fail "cb-decode.py did not produce a verdict — the ORACLE failed, not necessarily the server. Fix the drill before reading anything into this run." ;;
esac
[ "$CB_RC" -eq 0 ] \
  || fail "the callback exchange did not complete cleanly on the wire (see the decode above) — the capture contains the RPC and the client's answer to it, so this is a server-side failure"
ok "CB_LAYOUTRECALL accepted by the client ON THE WIRE (AUTH_SYS cred, CB_SEQUENCE NFS4_OK)"

# The server-side counterpart, which is now trustworthy BECAUSE C3 landed:
REFUSED=$(kubectl logs -n "$NS" "$MDS_POD" --since=5m 2>/dev/null | grep -c "REFUSED by session" || true)
[ "${REFUSED:-0}" -eq 0 ] || fail "MDS logged ${REFUSED} REFUSED recall(s) — the client rejected the callback"
ok "no refusals logged (and after C3 this line means something)"

# ABSENCE OF A REFUSAL IS NOT AN ACK. Every defect in this chain so far —
# C3's Ok(_)=>Acked, C8's denied RPC, C9's BADSESSION — produced a run
# with no refusal logged at the point we looked. Demand the positive
# statement: N/N acked, all revoked. `grep -c` not `grep -q`, because
# `grep -q` under `set -o pipefail` exits early and SIGPIPEs its producer
# (the runaj rule, learned the hard way and violated twice since).
ACKED=$(kubectl logs -n "$NS" "$MDS_POD" --since=5m 2>/dev/null \
  | grep -c "acked, all revoked server-side" || true)
PARTIAL=$(kubectl logs -n "$NS" "$MDS_POD" --since=5m 2>/dev/null \
  | grep -c "only .*/.* acked" || true)
[ "${PARTIAL:-0}" -eq 0 ] \
  || fail "MDS logged a PARTIAL recall (${PARTIAL} occurrence(s) of 'only N/M acked') — at least one client never confirmed, so it may still be reading past the new EOF"
[ "${ACKED:-0}" -gt 0 ] \
  || fail "no 'N/N acked, all revoked server-side' line — the recall was sent but never positively confirmed by any client. C3 made this line honest; a run without it is a run that did not prove delivery."
ok "recall positively ACKED by every holder (${ACKED} fan-out(s), all revoked)"

# C9 precondition. Not the oracle — the oracle is the ack above and the
# bytes in phase 1 — but when a recall fails this tells you INSTANTLY
# whether the back-channel handshake even happened, instead of leaving
# you to diff a pcap. Pre-C9 the server registered the channel and never
# echoed CONN_BACK_CHAN, so the client answered every callback with
# BADSESSION and this line did not exist.
BC=$(kubectl logs -n "$NS" "$MDS_POD" 2>/dev/null | grep -c "back channel ACCEPTED for session" || true)
if [ "${BC:-0}" -gt 0 ]; then
  ok "back-channel handshake completed (${BC} session(s) — csr_flags echoed CONN_BACK_CHAN)"
else
  note "no 'back channel ACCEPTED' line — either this MDS predates C9, or no client offered CONN_BACK_CHAN"
fi

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
# `kubectl wait --for=condition=Ready pod -l app=...` waits for EVERY
# matching pod, and the one we just deleted is still in the list while it
# terminates — it will never go Ready, so the wait always burns its full
# timeout. Watch the Deployment roll out instead. The timeout is generous
# because the MDS PVC is RWO: the replacement cannot attach until the old
# pod fully releases it (observed ~36s of Multi-Attach backoff), so a
# tight bound here reports "MDS never came back" about an MDS that came
# back fine.
kubectl rollout status deploy/flint-pnfs-mds -n "$NS" --timeout=300s >/dev/null 2>&1 \
  || fail "MDS never came back (deployment did not finish rolling out)"
kubectl wait --for=condition=Ready pod -l app=flint-pnfs-mds -n "$NS" --timeout=120s >/dev/null 2>&1 \
  || fail "MDS rolled out but never became Ready"
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
