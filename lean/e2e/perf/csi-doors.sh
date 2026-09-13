#!/usr/bin/env bash
# csi-doors.sh — the two doors AS DEPLOYED: the chart, the s3.csi.chert.us
# node plugin, a worker pod per volume, and a tenant pod that reads and
# writes through the plugin's mount. Runs ON the worker node as root, like
# doors.sh, with kubectl against the control plane.
#
#   csi-doors.sh setup        kubectl, the HEAD lean worker image, the three charts, the drill namespace, CRs, the reader pod
#   csi-doors.sh run [reps]   READ: Ld (lean workspace: pod created -> Ready, and the syncer's own phase line) · Pd-32 · Pdw-32
#   csi-doors.sh write [reps] WRITE: Ld-W (.flint/publish -> ack, from the tenant pod) · Pd-W (32-wide cp into the mount)
#   csi-doors.sh clean        delete the drill namespace and its CRs (the stack itself goes with the cluster)
#
# Same seeded objects as doors.sh (<PREFIX>/<w>/files/), same cold rule
# (drop_caches on the host before every arm), same guards (bytes/files
# as the pod sees them; writes as S3 LISTS them), plus one control: the
# kubectl-exec round trip, so it can be subtracted from the short arms.
set -uo pipefail

MODE="${1:-run}"
REPS="${2:-3}"
: "${BUCKET:?set BUCKET}"
: "${PREFIX:=ranged-drill}"
: "${ROOT:=/mnt/nvme/drill}"
: "${RIG:=/mnt/nvme/rig}"
: "${AWS_REGION:=us-west-1}"
: "${CP:=https://172.31.17.214:6443}"
: "${LEAN_TAG:=head-f7d44444}"
export AWS_REGION AWS_DEFAULT_REGION="$AWS_REGION" AWS_MAX_ATTEMPTS=5
WORKLOADS="${WORKLOADS:-big small mixed}"
NS=perf-drill
mkdir -p "$ROOT/results" "$RIG"
TS=$(date -u +%Y%m%d-%H%M%S)
RESULTS="$ROOT/results/csi-$MODE-$TS.tsv"

declare -A WANT_FILES=( [big]=6           [small]=20000     [mixed]=2001       )
declare -A WANT_BYTES=( [big]=6442450944  [small]=163840000 [mixed]=4327735296 )

log() { echo "$(date -u +%H:%M:%S) $*" >&2; }
now_ms() { echo $(( $(date +%s%N) / 1000000 )); }
drop_caches() { sync; echo 3 > /proc/sys/vm/drop_caches; }
row() { printf '%s\t%s\t%s\t%s\t%s\t%s\t%s\t%s\n' "$@" | tee -a "$RESULTS"; }
K() { "$RIG/kubectl" --server="$CP" --token="$(cat "$RIG/token")" --insecure-skip-tls-verify "$@"; }

# ── the pieces that live in the cluster ──────────────────────────────
pt_cr() { # <name> <keyPrefix> <readOnly>
  cat <<EOF
apiVersion: chert.us/v1alpha1
kind: FlintPassthroughMount
metadata: { name: $1, namespace: $NS }
spec:
  bucket: $BUCKET
  keyPrefix: $2
  region: $AWS_REGION
  readOnly: $3
  uid: 1001
  gid: 1001
  consumers: { serviceAccounts: [reader] }
  identity: { mode: broker }
EOF
}
lean_cr() { # <name> <keyPrefix> <projectId>
  cat <<EOF
apiVersion: chert.us/v1alpha1
kind: FlintLeanWorkspace
metadata: { name: $1, namespace: $NS }
spec:
  projectId: $3
  bucket: $BUCKET
  keyPrefix: $2
  uid: 1001
  floorSecs: 60
  # The v1.51.0 default, named explicitly: this cluster's 1.50.0 CRD would persist 512 (the old default), which
  # OOM-kills the worker under its 1Gi limit on the 1 GiB objects of big (finding 4 in the results doc).
  fetchInflightMb: 128
  consumers: { serviceAccounts: [reader] }
  identity: { mode: broker }
EOF
}
# A tenant pod: uid 1001 (the CRs' uid), the drill scripts from a
# ConfigMap, and either csi volumes or a csi volume + the seed tree.
pod() { # <name> <volumes-yaml> <mounts-yaml>
  cat <<EOF
apiVersion: v1
kind: Pod
metadata: { name: $1, namespace: $NS }
spec:
  serviceAccountName: reader
  restartPolicy: Never
  terminationGracePeriodSeconds: 5
  securityContext: { runAsUser: 1001, runAsGroup: 1001, runAsNonRoot: true, fsGroup: 1001 }
  containers:
    - name: t
      image: ubuntu:24.04
      command: [sleep, infinity]
      volumeMounts:
        - { name: drill, mountPath: /drill }
$3
  volumes:
    - name: drill
      configMap: { name: drill, defaultMode: 0755 }
$2
EOF
}
pt_vol()   { printf '    - name: %s\n      csi: { driver: s3.csi.chert.us, volumeAttributes: { chert.us/mount: %s } }\n' "$1" "$2"; }
lean_vol() { printf '    - name: %s\n      csi: { driver: s3.csi.chert.us, volumeAttributes: { chert.us/workspace: %s } }\n' "$1" "$2"; }
seed_vol() { printf '    - name: seed\n      hostPath: { path: %s, type: Directory }\n' "$1"; }
mnt()      { printf '        - { name: %s, mountPath: %s%s }\n' "$1" "$2" "${3:+, readOnly: true}"; }

setup() {
  log "kubectl + token"
  [ -x "$RIG/kubectl" ] || curl -fsSL https://dl.k8s.io/release/v1.34.11/bin/linux/amd64/kubectl -o "$RIG/kubectl"
  chmod +x "$RIG/kubectl"
  aws s3 cp "s3://$BUCKET/_rig/csi/token" "$RIG/token" --quiet
  K get nodes -o wide | cut -c1-80 >&2

  log "the HEAD lean worker image, into containerd"
  if ! ctr -n k8s.io images ls -q | grep -q "flint-s3-worker-lean:$LEAN_TAG"; then
    aws s3 cp "s3://$BUCKET/_rig/csi/lean-images-$LEAN_TAG.tar.gz" - | gunzip | ctr -n k8s.io images import - >&2
  fi
  ctr -n k8s.io images ls -q | grep -E "flint-sync:$LEAN_TAG|flint-s3-worker-lean:$LEAN_TAG" >&2

  log "the plugin's state dir on the NVMe: the lean tree is a loop-mounted ext4 image under the plugin dir, and the root disk is EBS"
  mkdir -p /mnt/nvme/plugin /var/lib/kubelet/plugins/s3.csi.chert.us
  mountpoint -q /var/lib/kubelet/plugins/s3.csi.chert.us || mount --bind /mnt/nvme/plugin /var/lib/kubelet/plugins/s3.csi.chert.us
  findmnt -n /var/lib/kubelet/plugins/s3.csi.chert.us >&2

  log "the charts, as rendered"
  for f in s3csi lean passthrough; do aws s3 cp "s3://$BUCKET/_rig/csi/$f.yaml" "$RIG/$f.yaml" --quiet; done
  K apply -f "$RIG/passthrough.yaml" >&2
  K apply -f "$RIG/lean.yaml" >&2
  log "the broker's static credential, minted from THIS node's IMDS (the CNI blocks pod IMDS, so ambient is dead here; the broker holds the node role's temp creds instead — expire ~5h, past this drill)"
  TOK=$(curl -sS -X PUT "http://169.254.169.254/latest/api/token" -H "X-aws-ec2-metadata-token-ttl-seconds: 300")
  ROLE=$(curl -sS -H "X-aws-ec2-metadata-token: $TOK" http://169.254.169.254/latest/meta-data/iam/security-credentials/)
  CRD=$(curl -sS -H "X-aws-ec2-metadata-token: $TOK" "http://169.254.169.254/latest/meta-data/iam/security-credentials/$ROLE")
  AKID=$(python3 -c 'import json,sys;print(json.load(sys.stdin)["AccessKeyId"])' <<<"$CRD")
  SAK=$(python3 -c 'import json,sys;print(json.load(sys.stdin)["SecretAccessKey"])' <<<"$CRD")
  STOK=$(python3 -c 'import json,sys;print(json.load(sys.stdin)["Token"])' <<<"$CRD")
  [ -n "$AKID" ] && [ -n "$SAK" ] && [ -n "$STOK" ] || { echo "IMDS creds empty — cannot mint broker secret" >&2; return 1; }
  K -n flint-system create secret generic broker-creds \
    --from-literal=AWS_ACCESS_KEY_ID="$AKID" --from-literal=AWS_SECRET_ACCESS_KEY="$SAK" --from-literal=AWS_SESSION_TOKEN="$STOK" \
    --dry-run=client -o yaml | K apply -f - >&2
  K apply -f "$RIG/s3csi.yaml" >&2
  K -n flint-system rollout status deploy/flint-s3-broker --timeout=180s >&2
  K -n flint-system rollout status ds/flint-s3-csi-node --timeout=300s >&2
  K -n flint-system rollout status deploy/flint-lean --timeout=300s >&2

  log "the drill namespace (privileged: the write pods mount the seed tree as a hostPath), SA, scripts, CRs"
  K apply -f - >&2 <<EOF
apiVersion: v1
kind: Namespace
metadata:
  name: $NS
  labels: { pod-security.kubernetes.io/enforce: privileged }
---
apiVersion: v1
kind: ServiceAccount
metadata: { name: reader, namespace: $NS }
EOF
  K -n "$NS" create configmap drill --dry-run=client -o yaml \
      --from-file=p_read.sh=<(cat <<'EOS'
#!/bin/bash
# p_read.sh <dir> <par> -> "ms bytes files": the same 32-wide cat as doors.sh, listing built OUTSIDE the window
d=$1; par=$2; list=/tmp/list.$$
find "$d" -type f -not -path '*/.*' | sort > "$list"; n=$(wc -l < "$list")
t0=$(date +%s%N)
for (( i = 0; i < par; i++ )); do
  awk -v p="$par" -v k="$i" 'NR % p == k' "$list" | while IFS= read -r f; do cat -- "$f"; done > /dev/null &
done
wait
t1=$(date +%s%N)
b=$(xargs -a "$list" -d '\n' stat -c %s 2>/dev/null | awk '{s+=$1} END {print s+0}')
echo "$(( (t1 - t0) / 1000000 )) $b $n"
EOS
) --from-file=p_write.sh=<(cat <<'EOS'
#!/bin/bash
# p_write.sh <seed> <mount> -> "ms": 32-wide cp of the seed's files into the mount. mount-s3 completes each PUT at close.
seed=$1; d=$2; list=/tmp/wlist.$$
find "$seed" -type f -not -path '*/.*' | sort > "$list"
t0=$(date +%s%N)
for (( i = 0; i < 32; i++ )); do
  awk -v p=32 -v k="$i" 'NR % p == k' "$list" | while IFS= read -r f; do cp -- "$f" "$d/${f##*/}"; done &
done
wait
t1=$(date +%s%N)
echo "$(( (t1 - t0) / 1000000 ))"
EOS
) --from-file=l_publish.sh=<(cat <<'EOS'
#!/bin/bash
# l_publish.sh <workspace> -> "ms <ack json>": the boundary verb, timed from the write of .flint/publish to the ack file
cd "$1" || exit 1
rm -f .flint/publish.ack
t0=$(date +%s%N)
printf '{"nonce":"drill-%s"}' "$(date +%s)" > .flint/publish
until [ -f .flint/publish.ack ]; do sleep 0.1; done
t1=$(date +%s%N)
echo "$(( (t1 - t0) / 1000000 )) $(tr -d '\n' < .flint/publish.ack)"
EOS
) --from-file=tree.sh=<(cat <<'EOS'
#!/bin/bash
# tree.sh <dir> -> "bytes files", dot-paths excluded (the same rule as doors.sh)
echo "$(find "$1" -type f -not -path '*/.*' -printf '%s\n' | awk '{s+=$1} END {print s+0}') $(find "$1" -type f -not -path '*/.*' | wc -l)"
EOS
) | K apply -f - >&2
  for w in $WORKLOADS; do
    pt_cr "pt-$w" "$PREFIX/$w/files" true | K apply -f - >&2
    lean_cr "lean-$w" "$PREFIX/$w" "drill/$w" | K apply -f - >&2
  done

  log "the passthrough reader pod: one tenant pod, three mounts, three worker pods — and the image pulls, outside every window"
  { pod preader "$(for w in $WORKLOADS; do pt_vol "$w" "pt-$w"; done)" "$(for w in $WORKLOADS; do mnt "$w" "/mnt/$w"; done)"; } | K apply -f - >&2
  K -n "$NS" wait pod/preader --for=condition=Ready --timeout=600s >&2 || { log "SETUP FAILED: preader never became Ready"; return 1; }
  K -n flint-workers get pods -o wide | cut -c1-110 >&2
  for w in $WORKLOADS; do
    read -r b n <<<"$(K -n "$NS" exec preader -- bash /drill/tree.sh "/mnt/$w")"
    log "  preader sees $w: $n files, $b bytes"
  done
  log "a throwaway lean pod, so the first measured one pulls nothing"
  pod lean-warm "$(lean_vol ws lean-small)" "$(mnt ws /workspace)" | K apply -f - >&2
  # FATAL on purpose: the first run of this rig printed SETUP DONE / exit 0 with the warm pod stuck on a
  # crashlooping worker (301 PermanentRedirect: wrong plugin region), and the caller launched the drill on it.
  K -n "$NS" wait pod/lean-warm --for=condition=Ready --timeout=900s >&2 || { K -n flint-workers logs "$(K -n flint-workers get pods -l chert.us/mode=lean -o name | head -1)" 2>&1 | tail -5 >&2; K -n "$NS" delete pod lean-warm --wait=false >&2; log "SETUP FAILED: lean-warm never became Ready"; return 1; }
  K -n flint-workers get pods -o wide | cut -c1-110 >&2
  K -n flint-workers logs "$(K -n flint-workers get pods -l chert.us/mode=lean -o name | head -1)" 2>&1 | grep -E "flint-sync: (phase|barrier|checkout)|error|refus" | tail -5 >&2
  K -n "$NS" delete pod lean-warm --wait=true --timeout=600s >&2
  lean_worker_gone
  log "SETUP DONE"
}

lean_worker_gone() { until [ -z "$(K -n flint-workers get pods -l chert.us/mode=lean -o name 2>/dev/null)" ]; do sleep 2; done; }

# ── the arms ────────────────────────────────────────────────────────
lean_read() { # <rep> <w>
  local rep="$1" w="$2" name="lean-$w"
  drop_caches
  local t0 t1
  t0=$(now_ms)
  pod "$name" "$(lean_vol ws "lean-$w")" "$(mnt ws /workspace)" | K apply -f - > /dev/null
  K -n "$NS" wait "pod/$name" --for=condition=Ready --timeout=900s > /dev/null \
    || { log "LEAN POD NOT READY [$w]"; K -n "$NS" describe pod "$name" | tail -15 >&2; row "$rep" "$w" Ld FAIL - - - -; K -n "$NS" delete pod "$name" --wait=true --timeout=600s >/dev/null 2>&1; lean_worker_gone; return 1; }
  t1=$(now_ms)
  local worker phase fetch ranged b n
  worker=$(K -n flint-workers get pods -l chert.us/mode=lean -o name | head -1)
  phase=$(K -n flint-workers logs "$worker" 2>/dev/null | grep -F 'flint-sync: phase' | tail -1)
  fetch=$(sed -n 's/.*fetch=\([0-9.]*\)s.*/\1/p' <<<"$phase")
  ranged=$(sed -n 's/.*ranged=\([0-9]*\).*/\1/p' <<<"$phase")
  read -r b n <<<"$(K -n "$NS" exec "$name" -- bash /drill/tree.sh /workspace)"
  log "  Ld[$w]: ready in $(( t1 - t0 )) ms; $phase"
  row "$rep" "$w" Ld $(( t1 - t0 )) "$(awk -v f="${fetch:-0}" 'BEGIN { printf "%d", f * 1000 }')" "$b" "$n" "${ranged:--}"
  K -n "$NS" delete pod "$name" --wait=true --timeout=600s > /dev/null
  lean_worker_gone
}
pt_read() { # <rep> <w>
  local rep="$1" w="$2" ms b n
  drop_caches
  read -r ms b n <<<"$(K -n "$NS" exec preader -- bash /drill/p_read.sh "/mnt/$w" 32)"; row "$rep" "$w" Pd-32 "$ms" - "$b" "$n" -
  read -r ms b n <<<"$(K -n "$NS" exec preader -- bash /drill/p_read.sh "/mnt/$w" 32)"; row "$rep" "$w" Pdw-32 "$ms" - "$b" "$n" -
}
exec_control() { # the kubectl exec round trip, so the short arms can be read net of it
  local t0 t1
  t0=$(now_ms); K -n "$NS" exec preader -- true; t1=$(now_ms)
  echo $(( t1 - t0 ))
}

s3_landed() { aws s3 ls --recursive --summarize "s3://$BUCKET/$1/" 2>/dev/null | awk '/Total Objects:/ {n=$3} /Total Size:/ {b=$3} END {printf "%d %d", n+0, b+0}'; }

lean_write() { # <rep> <w>
  local rep="$1" w="$2" cr="lean-w-$w-r$rep" name="lean-w-$w" pfx="w-Ld-r$rep-$TS/$w"
  lean_cr "$cr" "$pfx" "drill/$pfx" | K apply -f - > /dev/null
  # $(...) strips the trailing newline, so two fragments must be joined by an explicit one (the read pods
  # have a single volume and never met this; the first write run rendered both on one line and failed to parse).
  pod "$name" "$(lean_vol ws "$cr")"$'\n'"$(seed_vol "$ROOT/seed-$w")" "$(mnt ws /workspace)"$'\n'"$(mnt seed /seed ro)" | K apply -f - > /dev/null
  K -n "$NS" wait "pod/$name" --for=condition=Ready --timeout=900s > /dev/null \
    || { log "LEAN WRITE POD NOT READY [$w]"; K -n "$NS" describe pod "$name" | tail -15 >&2; row "$rep" "$w" Ld-W FAIL - - - -; K -n "$NS" delete pod "$name" --wait=true --timeout=600s >/dev/null 2>&1; lean_worker_gone; return 1; }
  # the seed into the workspace: local disk to local disk, NOT timed
  K -n "$NS" exec "$name" -- bash -c 'cp -a /seed/. /workspace/ && sync' > /dev/null
  drop_caches
  local ms ack n b
  read -r ms ack <<<"$(K -n "$NS" exec "$name" -- bash /drill/l_publish.sh /workspace)"
  log "  Ld-W[$w]: $ms ms; ack=$(cut -c1-160 <<<"$ack")"
  read -r n b <<<"$(s3_landed "$pfx/files")"
  row "$rep" "$w" Ld-W "$ms" - "${b:-0}" "${n:-0}" -
  K -n "$NS" delete pod "$name" --wait=true --timeout=600s > /dev/null
  lean_worker_gone
  K -n "$NS" delete flintleanworkspace "$cr" > /dev/null
}
pt_write() { # <rep> <w>
  local rep="$1" w="$2" cr="pt-w-$w-r$rep" name="pt-w-$w" pfx="w-Pd-r$rep-$TS/$w"
  pt_cr "$cr" "$pfx/files" false | K apply -f - > /dev/null
  pod "$name" "$(pt_vol m "$cr")"$'\n'"$(seed_vol "$ROOT/seed-$w")" "$(mnt m /mnt/w)"$'\n'"$(mnt seed /seed ro)" | K apply -f - > /dev/null
  K -n "$NS" wait "pod/$name" --for=condition=Ready --timeout=600s > /dev/null \
    || { log "PT WRITE POD NOT READY [$w]"; K -n "$NS" describe pod "$name" | tail -15 >&2; row "$rep" "$w" Pd-W FAIL - - - -; K -n "$NS" delete pod "$name" --wait=true --timeout=600s >/dev/null 2>&1; return 1; }
  drop_caches
  local ms n b
  ms=$(K -n "$NS" exec "$name" -- bash /drill/p_write.sh /seed /mnt/w)
  read -r n b <<<"$(s3_landed "$pfx/files")"
  log "  Pd-W[$w]: $ms ms; S3 lists $n files / $b bytes"
  row "$rep" "$w" Pd-W "$ms" - "${b:-0}" "${n:-0}" -
  K -n "$NS" delete pod "$name" --wait=true --timeout=600s > /dev/null
  K -n "$NS" delete flintpassthroughmount "$cr" > /dev/null
}

# ── guards + report ──────────────────────────────────────────────────
guards() {
  local fail=0 w
  for w in $WORKLOADS; do
    while IFS=$'\t' read -r rep ww arm wall fetch b n ranged; do
      [ "$wall" = FAIL ] && { echo "GUARD FAIL [$w/$arm rep $rep]: the arm failed" >&2; fail=1; continue; }
      [ "$b" = "${WANT_BYTES[$w]}" ] || { echo "GUARD FAIL [$w/$arm rep $rep]: $b bytes, seeded ${WANT_BYTES[$w]}" >&2; fail=1; }
      [ "$n" = "${WANT_FILES[$w]}" ] || { echo "GUARD FAIL [$w/$arm rep $rep]: $n files, seeded ${WANT_FILES[$w]}" >&2; fail=1; }
      if [ "$arm" = Ld ]; then
        # The deployed syncer runs as the daemon (`flint-sync run`), which prints no phase line — so the
        # deployed row has no fetch or ranged figure to judge. That is a limit of the log, not of the leg:
        # the bytes/files guard above is what catches skipped work, and the ranged path is a property of
        # the binary, which is the SAME binary the host arm `L-ship` ran under its own ranged guard.
        if [ "$ranged" = "-" ]; then echo "guard note [$w/Ld rep $rep]: no phase line from the daemon; ranged judged by L-ship's guard on the same binary" >&2
        elif [ "$w" = small ]; then [ "$ranged" = 0 ] || { echo "GUARD FAIL [$w/Ld]: ranged=$ranged on small objects" >&2; fail=1; }
        else [ "${ranged:-0}" -gt 0 ] 2>/dev/null || { echo "GUARD FAIL [$w/Ld]: ranged=$ranged — the ranged path never fired" >&2; fail=1; }; fi
      fi
    done < <(awk -F'\t' -v w="$w" '$2==w' "$RESULTS")
  done
  return $fail
}
report() {
  echo; echo "=== $1 — wall ms, RANGE over reps ($RESULTS) ==="
  printf '%-7s %-8s %10s %10s %7s %12s %8s %8s\n' workload arm min max spread fetch_ms files ranged
  local w arm
  for w in $WORKLOADS; do
    for arm in $(awk -F'\t' -v w="$w" '$2==w {print $3}' "$RESULTS" | awk '!s[$0]++'); do
      awk -F'\t' -v w="$w" -v a="$arm" '
        $2==w && $3==a && $4!="FAIL" { if (min=="" || $4<min) min=$4; if ($4>max) max=$4;
                                        if ($5!="-") { if (fmin=="" || $5<fmin) fmin=$5; if ($5>fmax) fmax=$5 }
                                        n=$7; r=$8 }
        END { if (min!="") printf "%-7s %-8s %10d %10d %6.1f%% %12s %8s %8s\n", w, a, min, max, (max-min)/min*100,
                     (fmin=="" ? "-" : fmin "-" fmax), n, r }' "$RESULTS"
    done
  done
  awk -F'\t' '$3=="ctl-exec" {printf "ctl-exec rep %s: kubectl exec round trip %d ms\n", $1, $4}' "$RESULTS"
}
upload_results() { aws s3 cp "$RESULTS" "s3://$BUCKET/_rig/results/$(basename "$RESULTS")" --quiet 2>/dev/null || true; }

case "$MODE" in
  setup) setup ;;
  run)
    echo "results -> $RESULTS" >&2
    for rep in $(seq 1 "$REPS"); do
      for w in $WORKLOADS; do
        log "rep $rep / $w"
        lean_read "$rep" "$w"
        pt_read "$rep" "$w"
      done
      row "$rep" - ctl-exec "$(exec_control)" - - - -
      upload_results
    done
    if guards; then echo "GUARDS PASSED"; report "CSI READ"; else echo "GUARDS FAILED — do not quote these numbers." >&2; report "CSI READ (VOID)"; upload_results; exit 1; fi
    upload_results ;;
  write)
    echo "results -> $RESULTS" >&2
    for rep in $(seq 1 "$REPS"); do
      for w in $WORKLOADS; do
        log "write rep $rep / $w"
        lean_write "$rep" "$w"
        pt_write "$rep" "$w"
      done
      upload_results
    done
    if guards; then echo "GUARDS PASSED"; report "CSI WRITE"; else echo "GUARDS FAILED — do not quote these numbers." >&2; report "CSI WRITE (VOID)"; upload_results; exit 1; fi
    upload_results ;;
  clean)
    K delete namespace "$NS" --wait=true --timeout=600s >&2 ;;
  *) echo "usage: $0 {setup|run [reps]|write [reps]|clean}" >&2; exit 2 ;;
esac
