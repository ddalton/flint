# Shared helpers for the pNFS k8s failure drills (durable-DS plan
# Phase 4). Each drill sources this. Requirements: KUBECONFIG pointing
# at a cluster with the chart's pNFS fleet (pnfs.enabled +
# pnfs.server.enabled), the flint-pnfs StorageClass applied, and
# busybox:1.36 pullable.
#
# Env knobs (defaults suit a 3-DS fleet):
#   NS           namespace of the fleet        (flint-system)
#   CLIENT_NODE  node for the writer pod       (required)
#   N_FILES      writer file count             (60)
#   FILE_MB      MiB per file                  (4)
#   DS_SAMPLES   blocks/file the DS check opens (16)
#
# The writer's payload is offset-stamped (stamp.awk), not zeros, and
# every drill therefore gets a second oracle: `verify_ds_stripes` asks
# a DS whether the bytes it holds are the bytes it owns. See stamp.awk
# for why zeros made all of this unfalsifiable.

NS=${NS:-flint-system}
N_FILES=${N_FILES:-60}
FILE_MB=${FILE_MB:-4}

# Payload geometry — see stamp.awk. Every 4 KiB block names its own
# offset and its own file, so a block that arrived from the wrong DS,
# at the wrong offset, or out of another file identifies itself.
#
# This replaces a payload of zeros. A DS stores its slice of a striped
# file sparsely and the ranges owned by other DSes read back as zeros,
# so a zero payload made "correct data" and "no data at all" the same
# bytes — every drill here passed regardless of the stripe map. See the
# header of stamp.awk for the full argument.
STAMP_BLK=4096
STAMP_BLOCKS=$(( FILE_MB * 1024 * 1024 / STAMP_BLK ))
# Blocks the DS-side check opens per file (see ds-stripe-check.sh).
DS_SAMPLES=${DS_SAMPLES:-16}

# Drills `cd` to their own directory before sourcing, so $0 is the safe
# fallback wherever BASH_SOURCE is unavailable under `set -u`.
DRILL_DIR=$(cd "$(dirname "${BASH_SOURCE[0]:-$0}")" && pwd)

step() { printf '\n▶ %s\n' "$*"; }
ok()   { printf '  ✓ %s\n' "$*"; }
note() { printf '  · %s\n' "$*"; }
fail() { printf '\n✗ %s\n' "$*" >&2; exit 1; }

need_env() {
  [ -n "${KUBECONFIG:-}" ] || fail "KUBECONFIG not set"
  [ -n "${CLIENT_NODE:-}" ] || fail "CLIENT_NODE not set (writer pod placement)"
  [ -f "$DRILL_DIR/stamp.awk" ] || fail "stamp.awk not found next to lib.sh"
  [ -f "$DRILL_DIR/ds-stripe-check.sh" ] || fail "ds-stripe-check.sh not found next to lib.sh"
  [ $(( FILE_MB * 1024 * 1024 % STAMP_BLK )) -eq 0 ] \
    || fail "FILE_MB=${FILE_MB} is not a whole number of ${STAMP_BLK}B blocks"
}

fleet_healthy() { # asserts every DS pod Ready and MDS Ready
  local not_ready
  not_ready=$(kubectl get pods -n "$NS" -l app=flint-pnfs-ds \
    -o jsonpath='{range .items[*]}{.metadata.name}={.status.containerStatuses[0].ready} {end}' \
    | tr ' ' '\n' | grep -c "=false" || true)
  [ "${not_ready:-0}" -eq 0 ] || fail "DS pods not Ready"
  kubectl wait --for=condition=Ready pod -l app=flint-pnfs-mds -n "$NS" --timeout=30s >/dev/null \
    || fail "MDS not Ready"
  ok "fleet healthy"
}

make_writer() { # <name>  — PVC + pod on CLIENT_NODE, mounts at /data
  local name=$1
  kubectl apply -f - <<EOF >/dev/null
apiVersion: v1
kind: PersistentVolumeClaim
metadata: {name: ${name}, namespace: default}
spec:
  accessModes: ["ReadWriteOnce"]
  storageClassName: flint-pnfs
  resources: {requests: {storage: 4Gi}}
---
apiVersion: v1
kind: Pod
metadata: {name: ${name}, namespace: default}
spec:
  restartPolicy: Never
  nodeSelector: {kubernetes.io/hostname: ${CLIENT_NODE}}
  containers:
    - name: w
      image: busybox:1.36
      command: ["sleep", "3600"]
      volumeMounts: [{name: d, mountPath: /data}]
  volumes:
    - name: d
      persistentVolumeClaim: {claimName: ${name}}
EOF
  kubectl wait --for=condition=Ready "pod/${name}" --timeout=180s >/dev/null \
    || fail "writer pod ${name} never became Ready"
  ok "writer ${name} mounted on ${CLIENT_NODE}"
}

install_stamper() { # <pod>  — ship the payload generator in and prove it runs
  local pod=$1 n
  kubectl exec -i "$pod" -- sh -c 'cat > /tmp/stamp.awk' < "$DRILL_DIR/stamp.awk" \
    || fail "could not install stamp.awk into ${pod}"
  # A generator that failed to land would write empty files whose shas
  # all agree with each other — the exact silent pass this whole change
  # exists to remove. Prove it emits a block before trusting it.
  n=$(kubectl exec "$pod" -- sh -c "awk -v fid=probe -v blocks=1 -f /tmp/stamp.awk | wc -c" 2>/dev/null | tr -d ' \r\n')
  [ "$n" = "$STAMP_BLK" ] \
    || fail "stamp.awk produced '${n}' bytes, want ${STAMP_BLK}, in ${pod} (no busybox awk?)"
}

start_load() { # <pod> [prefix]  — N_FILES stamped writes, progress + status in pod /tmp
  local pod=$1 pfx=${2:-d}
  install_stamper "$pod"
  # Generate locally, then dd onto the mount at bs=1M: the payload
  # changes, the write profile the drill is timing does not. The
  # expected sha is taken from the GENERATOR's output, never from
  # reading /data back, so it is an independent expectation rather
  # than a restatement of whatever the mount happens to return.
  kubectl exec "$pod" -- sh -c "rm -f /tmp/st /tmp/prog /tmp/sums /tmp/src /data/${pfx}-*.bin; \
    (for i in \$(seq 1 $N_FILES); do \
       awk -v fid=${pfx}-\$i -v blocks=$STAMP_BLOCKS -f /tmp/stamp.awk > /tmp/src \
         || { echo FAIL > /tmp/st; exit 1; }; \
       echo \"\$i \$(sha256sum /tmp/src | cut -d' ' -f1)\" >> /tmp/sums; \
       dd if=/tmp/src of=/data/${pfx}-\$i.bin bs=1M count=$FILE_MB 2>/dev/null \
         || { echo FAIL > /tmp/st; exit 1; }; \
       sync; echo \"\$i \$(date +%s)\" >> /tmp/prog; sleep 0.2; \
     done; echo OK > /tmp/st) & echo started" >/dev/null || fail "could not start writer load"
  ok "load started (${N_FILES} × ${FILE_MB} MiB, offset-stamped, prefix ${pfx}-)"
}

wait_load() { # <pod> <budget_s>  — sets LOAD_STATUS
  local pod=$1 budget=$2 i st=""
  for i in $(seq 1 $(( budget / 5 ))); do
    st=$(kubectl exec "$pod" -- cat /tmp/st 2>/dev/null || true)
    [ -n "$st" ] && break
    sleep 5
  done
  LOAD_STATUS=${st:-timeout}
}

max_stall() { # <pod>  — prints max inter-file gap (s) from /tmp/prog
  kubectl exec "$1" -- cat /tmp/prog 2>/dev/null | awk '
    NR>1 { gap=$2-prev; if (gap>max) max=gap } { prev=$2 } END { print max+0 }'
}

verify_load() { # <pod> [prefix]  — every file must equal the stamped stream it was written from
  local pod=$1 pfx=${2:-d} out
  # Driven off /tmp/sums rather than `seq`, and the count is asserted:
  # a truncated or missing sums file must FAIL rather than verify the
  # handful of entries it still has and call that a pass.
  out=$(kubectl exec "$pod" -- sh -c "n=0; bad=0; \
    while read -r i want; do \
      n=\$((n+1)); \
      got=\$(sha256sum /data/${pfx}-\$i.bin 2>/dev/null | cut -d' ' -f1); \
      [ \"\$got\" = \"\$want\" ] && continue; \
      bad=\$((bad+1)); \
      echo \"MISMATCH ${pfx}-\$i want=\$want got=\${got:-<unreadable>}\"; \
      echo '  first 64B on disk (all-NUL means this block came back as a hole):'; \
      dd if=/data/${pfx}-\$i.bin bs=$STAMP_BLK count=1 2>/dev/null | head -c 64 | od -c | head -2; \
    done < /tmp/sums; \
    echo \"CHECKED \$n of $N_FILES files, \$bad mismatched\"; \
    [ \$n -eq $N_FILES ] && [ \$bad -eq 0 ] && echo VERIFY-OK" 2>&1)
  case "$out" in
    *VERIFY-OK*) ok "all ${N_FILES} files match their stamped payload" ;;
    *) printf '%s\n' "$out" >&2
       fail "stamped-payload verification failed (${pfx}-*)" ;;
  esac
}

# Runs ds-stripe-check.sh in one DS and leaves the report in
# DS_STRIPE_REPORT. Fails hard on a real mismatch — every caller wants
# that — but leaves "this DS showed no evidence" to the caller, because
# what it means depends on whether the drill targeted this DS or is
# sweeping the fleet.
_ds_stripe_probe() { # <ds-pod> <prefix>
  local ds=$1 pfx=$2
  DS_STRIPE_REPORT=$(kubectl exec -i -n "$NS" "$ds" -- sh -s -- \
        "$pfx" "$STAMP_BLK" "$STAMP_BLOCKS" "$DS_SAMPLES" \
        < "$DRILL_DIR/ds-stripe-check.sh" 2>&1) \
    || { printf '%s\n' "$DS_STRIPE_REPORT" >&2; fail "DS stripe check could not run in ${ds}"; }
  case "$DS_STRIPE_REPORT" in
    *STRIPE-MISMATCH*)
      printf '%s\n' "$DS_STRIPE_REPORT" >&2
      fail "${ds}: STRIPE-MAP FAILURE — a block is not where its own stamp says it belongs" ;;
  esac
}

verify_ds_stripes() { # <ds-pod> [prefix]  — a DS the drill deliberately targeted
  local ds=$1 pfx=${2:-d}
  _ds_stripe_probe "$ds" "$pfx"
  case "$DS_STRIPE_REPORT" in
    *STRIPES-OK*)
      ok "${ds} stripe map clean — $(printf '%s\n' "$DS_STRIPE_REPORT" | sed -n 's/^SCANNED //p')" ;;
    *)
      # The drill picked this DS on purpose and then found nothing on it
      # to check. That is not a pass — it is the drill grading itself on
      # a question it never asked.
      printf '%s\n' "$DS_STRIPE_REPORT" >&2
      fail "${ds}: targeted by this drill but holds no verifiable stripe — the run proves nothing about it (wrong TARGET_DS? fleet too narrow? stale pinned placements from an earlier run's file names?)" ;;
  esac
}

verify_all_ds_stripes() { # [prefix]  — fleet sweep; at least one DS must yield evidence
  local pfx=${1:-d} p n=0 witnessed=0
  for p in $(kubectl get pods -n "$NS" -l app=flint-pnfs-ds \
               -o jsonpath='{range .items[*]}{.metadata.name}{"\n"}{end}'); do
    n=$(( n + 1 ))
    _ds_stripe_probe "$p" "$pfx"
    case "$DS_STRIPE_REPORT" in
      *STRIPES-OK*) witnessed=$(( witnessed + 1 ))
        ok "${p} stripe map clean — $(printf '%s\n' "$DS_STRIPE_REPORT" | sed -n 's/^SCANNED //p')" ;;
      *NO-FILES*) note "${p}: holds none of the drill's files" ;;
      *)          note "${p}: every sampled block was a hole" ;;
    esac
  done
  [ "$n" -gt 0 ] || fail "no flint-pnfs-ds pods found — the stripe sweep examined nothing"
  [ "$witnessed" -gt 0 ] \
    || fail "swept ${n} DS pods and not one held a verifiable stripe — the sweep proves nothing"
}

cleanup_writer() { # <name> — pod + PVC, tolerant
  kubectl delete pod "$1" --wait=true --timeout=120s >/dev/null 2>&1
  kubectl delete pvc "$1" --wait=false >/dev/null 2>&1
}

wait_pod_replaced() { # <ns> <pod> <old_uid> <timeout_s> — REPLACEMENT Ready
  # `kubectl wait --for=condition=Ready` right after a delete can match
  # the OLD Terminating pod (still Ready=true for a beat) — wait for
  # the UID to change first, then for readiness.
  local ns=$1 pod=$2 old_uid=$3 budget=$4 i uid ready
  for i in $(seq 1 $(( budget / 5 ))); do
    uid=$(kubectl get pod -n "$ns" "$pod" -o jsonpath='{.metadata.uid}' 2>/dev/null || true)
    ready=$(kubectl get pod -n "$ns" "$pod" -o jsonpath='{.status.containerStatuses[0].ready}' 2>/dev/null || true)
    [ -n "$uid" ] && [ "$uid" != "$old_uid" ] && [ "$ready" = "true" ] && return 0
    sleep 5
  done
  return 1
}
