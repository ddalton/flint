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

NS=${NS:-flint-system}
N_FILES=${N_FILES:-60}
FILE_MB=${FILE_MB:-4}
# sha256 of FILE_MB MiB of zeros — recomputed at drill start.
ZEROS_SHA=""

step() { printf '\n▶ %s\n' "$*"; }
ok()   { printf '  ✓ %s\n' "$*"; }
note() { printf '  · %s\n' "$*"; }
fail() { printf '\n✗ %s\n' "$*" >&2; exit 1; }

need_env() {
  [ -n "${KUBECONFIG:-}" ] || fail "KUBECONFIG not set"
  [ -n "${CLIENT_NODE:-}" ] || fail "CLIENT_NODE not set (writer pod placement)"
  ZEROS_SHA=$(dd if=/dev/zero bs=1M count="$FILE_MB" 2>/dev/null | shasum -a 256 | awk '{print $1}')
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

start_load() { # <pod>  — N_FILES sequential writes, progress + status in pod /tmp
  local pod=$1
  kubectl exec "$pod" -- sh -c "rm -f /tmp/st /tmp/prog /data/d-*.bin; \
    (for i in \$(seq 1 $N_FILES); do \
       dd if=/dev/zero of=/data/d-\$i.bin bs=1M count=$FILE_MB 2>/dev/null \
         || { echo FAIL > /tmp/st; exit 1; }; \
       sync; echo \"\$i \$(date +%s)\" >> /tmp/prog; sleep 0.2; \
     done; echo OK > /tmp/st) & echo started" >/dev/null || fail "could not start writer load"
  ok "load started (${N_FILES} × ${FILE_MB} MiB)"
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

verify_load() { # <pod>  — every file matches ZEROS_SHA
  local pod=$1
  kubectl exec "$pod" -- sh -c "bad=0; for i in \$(seq 1 $N_FILES); do \
      s=\$(sha256sum /data/d-\$i.bin | cut -d' ' -f1); \
      [ \"\$s\" = \"$ZEROS_SHA\" ] || { echo \"MISMATCH d-\$i \$s\"; bad=1; }; \
    done; [ \$bad -eq 0 ] && echo CHECKSUMS-OK" | grep CHECKSUMS-OK >/dev/null \
    || fail "checksum verification failed"
  ok "all ${N_FILES} checksums OK"
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
