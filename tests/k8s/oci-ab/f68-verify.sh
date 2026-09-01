#!/bin/bash
# F68 correctness fix — field verification (RED then GREEN on ONE cluster).
#
# The fix's unit test proves the gate refuses. This drill tests the two FIELD
# faces, which are DATA ops (READ/WRITE served from the striped file's stub):
#   face A (F-OCIAB-1): writer reads back its own small file -> NULs
#   face B (F-OCIAB-2): second client reads a large file -> page-aligned holes
# A third anomaly seen on runbx is recorded but NOT fatal and NOT the fix's
# target: a rename of a just-written file failing ENOENT while the file
# demonstrably exists server-side (namespace/dentry class, not the stub lane).
#
# Because F68c — whatever puts client data on the MDS lane — is still unowned,
# a clean GREEN alone proves nothing. RED is the mandatory anti-vacuity control:
#   RED   stock 1.43.0            -> a face MUST appear, else INCONCLUSIVE
#   GREEN swap ONLY the MDS image -> faces gone, ideally with loud refusals
#
# Usage: KC=<kubeconfig> ./f68-verify.sh red|green
set -u
KC=${KC:?kubeconfig}; K="kubectl --kubeconfig $KC"
LEG=${1:?red|green}
N=${N:-6}                       # files per class (more shots at a rare trigger)
k() { $K "$@"; }

mds_image() { k -n flint-system get deploy/flint-pnfs-mds -o jsonpath='{.spec.template.spec.containers[0].image}'; }
f68a()      { k -n flint-system logs deploy/flint-pnfs-mds --since=20m 2>/dev/null | grep -c "F68a" || echo 0; }
refusals()  { k -n flint-system logs deploy/flint-pnfs-mds --since=20m 2>/dev/null | grep -cE "⛔ (READ|WRITE) through MDS" || echo 0; }

k delete pod f68-w f68-r --ignore-not-found --wait=true >/dev/null 2>&1
k exec f68-ls -- true 2>/dev/null  # no-op; keeps kubectl warm

# ── writer: write at FINAL name (no rename in the critical path), then read
# ── back its OWN bytes immediately  = face A. Rename tested separately.
cat <<EOF | k apply -f - >/dev/null
apiVersion: v1
kind: Pod
metadata: {name: f68-w}
spec:
  restartPolicy: Never
  containers:
  - name: w
    image: busybox
    command: ["sh","-c"]
    args:
    - |
      D=/d/$LEG; rm -rf \$D; mkdir -p \$D
      faceA_bad=0; rename_retries=0; rename_fail=0
      i=1
      while [ \$i -le $N ]; do
        # --- small file, written then read back BY THE SAME CLIENT (face A)
        printf '2026-09-01T00:00:00Z' > \$D/small.\$i
        got=\$(od -An -tx1 -v \$D/small.\$i | tr -d ' \n')
        [ "\$got" = "323032362d30392d30315430303a30303a30305a" ] || {
          faceA_bad=\$((faceA_bad+1)); echo "FACE_A_HIT small.\$i got=\$got"; }
        # --- large file at final name, hash recorded for the reader
        dd if=/dev/urandom of=\$D/big.\$i bs=1M count=25 2>/dev/null
        sha256sum \$D/big.\$i | cut -d' ' -f1 > \$D/big.\$i.sha
        # --- rename anomaly probe (recorded, not fatal)
        printf 'x' > \$D/mv.\$i.tmp
        if ! mv \$D/mv.\$i.tmp \$D/mv.\$i 2>/dev/null; then
          rename_retries=\$((rename_retries+1)); sleep 2
          mv \$D/mv.\$i.tmp \$D/mv.\$i 2>/dev/null || rename_fail=\$((rename_fail+1))
        fi
        i=\$((i+1))
      done
      echo "WRITER faceA_bad=\$faceA_bad rename_retries=\$rename_retries rename_fail=\$rename_fail"
    volumeMounts: [{name: v, mountPath: /d}]
  volumes: [{name: v, persistentVolumeClaim: {claimName: f68-pvc}}]
EOF
k wait --for=jsonpath='{.status.phase}'=Succeeded pod/f68-w --timeout=600s >/dev/null 2>&1 || true
wout=$(k logs f68-w 2>/dev/null)
wnode=$(k get pod f68-w -o jsonpath='{.spec.nodeName}' 2>/dev/null)

# ── reader: DIFFERENT node, fresh mount, verify every large file = face B
cat <<EOF | k apply -f - >/dev/null
apiVersion: v1
kind: Pod
metadata: {name: f68-r}
spec:
  restartPolicy: Never
  affinity:
    nodeAffinity:
      requiredDuringSchedulingIgnoredDuringExecution:
        nodeSelectorTerms:
        - matchExpressions: [{key: kubernetes.io/hostname, operator: NotIn, values: ["$wnode"]}]
  containers:
  - name: r
    image: busybox
    command: ["sh","-c"]
    args:
    - |
      D=/d/$LEG; faceB_bad=0; missing=0
      i=1
      while [ \$i -le $N ]; do
        if [ -f \$D/big.\$i ] && [ -f \$D/big.\$i.sha ]; then
          e=\$(cat \$D/big.\$i.sha); a=\$(sha256sum \$D/big.\$i | cut -d' ' -f1)
          [ "\$e" = "\$a" ] || { faceB_bad=\$((faceB_bad+1)); echo "FACE_B_HIT big.\$i exp=\$e act=\$a"; }
        else
          missing=\$((missing+1))
        fi
        i=\$((i+1))
      done
      echo "READER faceB_bad=\$faceB_bad missing=\$missing"
    volumeMounts: [{name: v, mountPath: /d}]
  volumes: [{name: v, persistentVolumeClaim: {claimName: f68-pvc}}]
EOF
k wait --for=jsonpath='{.status.phase}'=Succeeded pod/f68-r --timeout=600s >/dev/null 2>&1 || true
rout=$(k logs f68-r 2>/dev/null)

echo "$wout"; echo "$rout"
cat <<SUM
── f68-verify $LEG ──────────────────────────────────────────
  mds_image     : $(mds_image)
  files/class   : $N      writer_node: ${wnode:-?}
  $(echo "$wout" | grep WRITER || echo "WRITER (no line — pod failed)")
  $(echo "$rout" | grep READER || echo "READER (no line — pod failed)")
  F68a lines    : $(f68a)      (0 ⇒ MDS lane not exercised ⇒ INCONCLUSIVE)
  loud refusals : $(refusals)  (>0 on GREEN = gate engaged AND trigger fired)
─────────────────────────────────────────────────────────────
SUM
