#!/usr/bin/env bash
# PHASE 0 — the BUY option, as a control.
#
# Gitea/Forgejo on a flint NFS-backed POSIX volume needs zero flint
# code. The design records why that is not enough (§12): their
# repository root must be a LOCAL PATH — only LFS, attachments and
# packages can live in S3 — so there is no per-push S3 durability, no
# idle-to-zero, no pod identity, and no legible export. Those are
# architectural and are checked by inspection.
#
# The one claim that needs measuring is fan-out, because it is the one
# a fleet actually feels: every clone comes off the server's NIC, with
# no bundle URI to move it to the object store. So this runs the SAME
# storm measurement against the same corpus and compares.
set -uo pipefail
NS=${NS:-buy}
N=${N:-100}; WIDTH=${WIDTH:-32}
# THE VOLUME. `flint-nfs` is the faithful control — a bought forge on a
# flint POSIX volume — but it needs trove's manual blobstore disk-init,
# which this cluster never got: the NFS hub refuses with
# `F30 REFUSAL (exit 57): export "/mnt/volume" has neither identity
# marker nor flint state`, and the pNFS pods sit Pending. That is a
# trove provisioning gap, not a forge finding.
#
# So VOLUME=local runs the control on an emptyDir instead. It gives up
# the "git over NFS is slow" half and keeps the half that decides
# anything: fan-out. A bought forge has no bundle URI, so every clone
# comes off its NIC whatever the disk underneath is.
SC=${SC:-flint-nfs}
VOLUME=${VOLUME:-local}
pass=0; fail=0
ok(){ pass=$((pass+1)); echo "  PASS  $1"; }
bad(){ fail=$((fail+1)); echo "  FAIL  $1"; }

kubectl create namespace "$NS" --dry-run=client -o yaml | kubectl apply -f - >/dev/null

if [ "$VOLUME" = nfs ]; then
cat <<YAML | kubectl apply -f - >/dev/null
apiVersion: v1
kind: PersistentVolumeClaim
metadata: { name: gitea-data, namespace: $NS }
spec:
  accessModes: [ReadWriteMany]
  storageClassName: $SC
  resources: { requests: { storage: 8Gi } }
YAML
VOLSPEC='persistentVolumeClaim: { claimName: gitea-data }'
else
VOLSPEC='emptyDir: {}'
fi
cat <<YAML | kubectl apply -f - >/dev/null
apiVersion: apps/v1
kind: Deployment
metadata: { name: gitea, namespace: $NS }
spec:
  replicas: 1
  selector: { matchLabels: { app: gitea } }
  template:
    metadata: { labels: { app: gitea } }
    spec:
      # The control must not be scheduled among the storm clients, for
      # the same reason the forge server is not.
      tolerations: []
      containers:
        - name: gitea
          image: gitea/gitea:1.22
          env:
            - { name: GITEA__database__DB_TYPE, value: sqlite3 }
            - { name: GITEA__security__INSTALL_LOCK, value: "true" }
            - { name: GITEA__server__ROOT_URL, value: "http://gitea.$NS.svc:3000/" }
            - { name: GITEA__server__DISABLE_SSH, value: "true" }
            - { name: GITEA__service__DISABLE_REGISTRATION, value: "false" }
            - { name: GITEA__repository__ENABLE_PUSH_CREATE_USER, value: "true" }
          ports: [ { containerPort: 3000 } ]
          volumeMounts: [ { name: data, mountPath: /data } ]
      volumes:
        - name: data
          $VOLSPEC
---
apiVersion: v1
kind: Service
metadata: { name: gitea, namespace: $NS }
spec:
  selector: { app: gitea }
  ports: [ { port: 3000, targetPort: 3000 } ]
YAML

echo "── waiting for the bought forge to come up on $SC ──"
kubectl wait -n "$NS" --for=condition=Available deploy/gitea --timeout=420s || { bad "gitea never became available"; kubectl get pvc,pods -n "$NS"; exit 1; }
ok "Gitea is running (volume=$VOLUME)"
[ "$VOLUME" = nfs ] && kubectl get pvc -n "$NS" gitea-data -o jsonpath='   PVC {.status.phase} {.spec.storageClassName}{"\n"}'
echo "   volume: $VOLUME" 

kubectl exec -n "$NS" deploy/gitea -- sh -c '
  su git -c "gitea admin user create --username drill --password drillpw123 --email d@x.y --admin --must-change-password=false" 2>&1 | tail -1' || true
echo ""
echo "══ $pass passed, $fail failed (setup) ══"
