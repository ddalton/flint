# Manifest emitters for the chaos harness. Each emit_* prints YAML to stdout,
# parameterized by env: NS (namespace), SC (storage class), ACCESS_MODE
# (ReadWriteOnce | ReadWriteMany), SCALE (pgbench scale, default 200).
#
# Design notes (see docs/attach-detach-campaign-2026-07.md):
#  - StatefulSet, not Deployment: at-most-one pod per PVC, so stuck-Terminating
#    windows surface honestly instead of hiding behind Multi-Attach errors.
#  - --data-checksums at initdb (torn/lost-page detection; initdb-only).
#  - No liveness probe: kubelet restart-fighting would muddy drill attribution.
#  - shareProcessNamespace + chaos sidecar so drills can SIGKILL the postmaster.
#  - pg-load carries REQUIRED anti-affinity to pg: its acked.log is the ground
#    truth for lost-acked-write detection and must survive pg-node kills.

emit_ns() {
cat <<EOF
apiVersion: v1
kind: Namespace
metadata:
  name: ${NS}
EOF
}

emit_secret() {
cat <<EOF
apiVersion: v1
kind: Secret
metadata: {name: pg-secret, namespace: ${NS}}
stringData:
  password: chaos-campaign-pw
EOF
}

emit_svc() {
cat <<EOF
apiVersion: v1
kind: Service
metadata: {name: pg, namespace: ${NS}}
spec:
  clusterIP: None
  selector: {app: pg}
  ports: [{port: 5432, name: pg}]
EOF
}

emit_sts() {
cat <<EOF
apiVersion: apps/v1
kind: StatefulSet
metadata: {name: pg, namespace: ${NS}}
spec:
  serviceName: pg
  replicas: 1
  selector: {matchLabels: {app: pg}}
  template:
    metadata: {labels: {app: pg}}
    spec:
      shareProcessNamespace: true
      terminationGracePeriodSeconds: 60
      containers:
        - name: postgres
          image: postgres:16-bookworm
          args: ["-c","shared_buffers=1GB","-c","max_wal_size=2GB",
                 "-c","checkpoint_timeout=30s","-c","synchronous_commit=on",
                 "-c","full_page_writes=on","-c","log_checkpoints=on"]
          env:
            - {name: PGDATA, value: /var/lib/postgresql/data/pgdata}
            - {name: POSTGRES_INITDB_ARGS, value: "--data-checksums"}
            - {name: POSTGRES_DB, value: bench}
            - name: POSTGRES_PASSWORD
              valueFrom: {secretKeyRef: {name: pg-secret, key: password}}
          ports: [{containerPort: 5432, name: pg}]
          readinessProbe:
            exec: {command: ["pg_isready","-U","postgres"]}
            periodSeconds: 5
            # 1s default budget flaps under heavy load (amcheck, -C
            # churn): NotReady empties the headless service's DNS,
            # starving every per-connection client (ledger oracle) —
            # the harness then measures its own probe artifact as a
            # "stall". 5s keeps readiness truthful to actual liveness.
            timeoutSeconds: 5
            failureThreshold: 3
          resources:
            requests: {cpu: "1", memory: 2Gi}
            limits: {cpu: "2", memory: 4Gi}
          volumeMounts:
            - {name: data, mountPath: /var/lib/postgresql/data}
        - name: chaos
          image: busybox:1.36
          command: ["sleep","infinity"]
  volumeClaimTemplates:
    - metadata: {name: data}
      spec:
        accessModes: ["${ACCESS_MODE}"]
        storageClassName: ${SC}
        resources: {requests: {storage: 20Gi}}
EOF
}

emit_init_job() {
cat <<EOF
apiVersion: batch/v1
kind: Job
metadata: {name: pg-init, namespace: ${NS}}
spec:
  backoffLimit: 10
  template:
    spec:
      restartPolicy: OnFailure
      containers:
        - name: init
          image: postgres:16-bookworm
          env:
            - {name: PGHOST, value: pg}
            - {name: PGUSER, value: postgres}
            - {name: PGDATABASE, value: bench}
            - name: PGPASSWORD
              valueFrom: {secretKeyRef: {name: pg-secret, key: password}}
          command:
            - bash
            - -ec
            - |
              until pg_isready -q; do sleep 3; done
              psql -c "CREATE EXTENSION IF NOT EXISTS amcheck"
              psql -c "CREATE TABLE IF NOT EXISTS ledger(seq bigint PRIMARY KEY, ts timestamptz DEFAULT now(), payload text)"
              pgbench -i -q -s ${SCALE:-200} bench
              echo INIT-DONE
EOF
}

emit_load() {
cat <<'LOADEOF' | sed "s/__NS__/${NS}/"
apiVersion: apps/v1
kind: Deployment
metadata: {name: pg-load, namespace: __NS__}
spec:
  replicas: 1
  selector: {matchLabels: {app: pg-load}}
  template:
    metadata: {labels: {app: pg-load}}
    spec:
      affinity:
        podAntiAffinity:
          requiredDuringSchedulingIgnoredDuringExecution:
            - labelSelector: {matchLabels: {app: pg}}
              topologyKey: kubernetes.io/hostname
      containers:
        - name: load
          image: postgres:16-bookworm
          env:
            - {name: PGHOST, value: pg}
            - {name: PGUSER, value: postgres}
            - {name: PGDATABASE, value: bench}
            - {name: PGCONNECT_TIMEOUT, value: "3"}
            - name: PGPASSWORD
              valueFrom: {secretKeyRef: {name: pg-secret, key: password}}
          volumeMounts:
            - {name: acked, mountPath: /acked}
          command:
            - bash
            - -c
            - |
              # Wait for schema (init job) before generating load.
              until psql -Atqc "SELECT 1 FROM ledger LIMIT 1" >/dev/null 2>&1; do
                echo "$(date +%s) waiting-for-init"; sleep 5
              done
              echo "$(date +%s) LOAD-START"
              # pgbench pressure loop: tolerates connection loss, timestamps
              # every exit so stall windows are reconstructable from logs.
              ( while true; do
                  pgbench -n -c 4 -j 2 -T 86400 -P 10 bench
                  echo "$(date +%s) PGBENCH-EXIT rc=$?"
                  sleep 2
                done ) &
              # Ledger oracle: one INSERT per iteration; only ACKed commits
              # are recorded to acked.log. seq seeds from MAX(seq) so pod
              # restarts never collide on the primary key.
              i=$(psql -Atqc "SELECT COALESCE(MAX(seq),0) FROM ledger" 2>/dev/null || echo 0)
              while true; do
                i=$((i+1))
                if out=$(psql -Atqc "INSERT INTO ledger(seq,payload) VALUES ($i, md5($i::text)) RETURNING seq" 2>/dev/null) \
                   && [ "$out" = "$i" ]; then
                  echo "$i $(date +%s)" >> /acked/acked.log
                else
                  echo "$i $(date +%s)" >> /acked/indeterminate.log
                fi
                sleep 0.2
              done
      volumes:
        - name: acked
          emptyDir: {}
LOADEOF
}

# Phase 3 only: second consumer of the SAME RWX PVC on a DIFFERENT node.
# Appends to its own flat-file ledger and read-verifies every write; any
# WITNESS-MISMATCH line in its log is a cross-node consistency failure.
emit_witness() {
cat <<EOF
apiVersion: apps/v1
kind: Deployment
metadata: {name: witness, namespace: ${NS}}
spec:
  replicas: 1
  selector: {matchLabels: {app: witness}}
  template:
    metadata: {labels: {app: witness}}
    spec:
      affinity:
        podAntiAffinity:
          requiredDuringSchedulingIgnoredDuringExecution:
            - labelSelector: {matchLabels: {app: pg}}
              topologyKey: kubernetes.io/hostname
      containers:
        - name: witness
          image: busybox:1.36
          command:
            - sh
            - -c
            - |
              i=0
              while true; do
                i=\$((i+1))
                echo "\$i \$(date +%s)" >> /mnt/witness.log
                tail -1 /mnt/witness.log | grep -q "^\$i " \
                  || echo "\$(date +%s) WITNESS-MISMATCH \$i"
                sleep 1
              done
          volumeMounts:
            - {name: shared, mountPath: /mnt}
      volumes:
        - name: shared
          persistentVolumeClaim: {claimName: data-pg-0}
EOF
}
